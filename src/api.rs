use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use crate::pb;
use crate::pb::kilnd_service_client::KilndServiceClient;

/// A backend-neutral sandbox request expressed in kilnd API semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRequest {
    pub image: String,
    pub memory_bytes: u64,
    pub cpu_millis: u32,
    pub pids: u32,
    pub disk_bytes: u64,
    pub network_none: bool,
    pub read_only_root: bool,
    pub writable_paths: Vec<String>,
    pub labels: BTreeMap<String, String>,
    /// Sent only when the daemon lists [`Feature::SandboxExpiry`]: an older
    /// daemon ignores the field silently. `None` never expires.
    pub expiry: Option<ExpiryPolicy>,
    /// Sent only when the daemon lists [`Feature::IdempotentCreate`]: an
    /// older daemon ignores the field and could create a second sandbox for
    /// a retry. `None` is an unkeyed create, which is never retried. A retry
    /// must send a request equal to the first, since kilnd answers any other
    /// request under a used key `request_id_conflict`.
    pub request_id: Option<String>,
}

/// The longest create request ID kilnd accepts.
pub const MAX_REQUEST_ID_BYTES: usize = 64;

/// Whether `id` is a create request ID kilnd accepts: 1 to 64 bytes, each
/// printable ASCII without space (`0x21`-`0x7e`).
#[must_use]
pub(crate) fn request_id_is_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REQUEST_ID_BYTES
        && id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

/// When kilnd may reclaim a sandbox on its own. Each field is whole
/// milliseconds; kilnd reads a zero field as its node default, which the
/// harness never sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpiryPolicy {
    /// Counted from the first `READY`, whatever the activity.
    pub max_lifetime_ms: u64,
    /// Counted from the last execution start or finish or `RenewSandbox`,
    /// and paused while an execution runs or is queued.
    pub idle_timeout_ms: u64,
}

/// A sandbox's effective expiry as kilnd reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExpiryStatus {
    /// Zero in a field: the sandbox does not expire that way.
    pub effective: Option<ExpiryPolicy>,
    /// Wall-clock projections computed when read; 0 means none.
    pub lifetime_expires_at_unix_ms: i64,
    pub idle_expires_at_unix_ms: i64,
    pub last_activity_at_unix_ms: i64,
    pub idle_paused: bool,
}

/// A sandbox's lifecycle state, as kilnd reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxState {
    Creating,
    Ready,
    Deleting,
    Deleted,
    Failed,
    Lost,
    Quarantined,
    /// A state this schema does not name, by its protobuf number;
    /// `Other(0)` when the state is unspecified.
    Other(i32),
}

impl SandboxState {
    #[must_use]
    pub fn from_proto(value: i32) -> Self {
        match pb::SandboxState::try_from(value) {
            Ok(pb::SandboxState::Creating) => Self::Creating,
            Ok(pb::SandboxState::Ready) => Self::Ready,
            Ok(pb::SandboxState::Deleting) => Self::Deleting,
            Ok(pb::SandboxState::Deleted) => Self::Deleted,
            Ok(pb::SandboxState::Failed) => Self::Failed,
            Ok(pb::SandboxState::Lost) => Self::Lost,
            Ok(pb::SandboxState::Quarantined) => Self::Quarantined,
            Ok(pb::SandboxState::Unspecified) | Err(_) => Self::Other(value),
        }
    }

    /// The name after `SANDBOX_STATE_`, as the schema spells it;
    /// `STATE_<n>` for a number this schema does not name.
    #[must_use]
    pub fn as_str(self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(match self {
            Self::Creating => "CREATING",
            Self::Ready => "READY",
            Self::Deleting => "DELETING",
            Self::Deleted => "DELETED",
            Self::Failed => "FAILED",
            Self::Lost => "LOST",
            Self::Quarantined => "QUARANTINED",
            Self::Other(value) => return std::borrow::Cow::Owned(format!("STATE_{value}")),
        })
    }
}

impl std::fmt::Display for SandboxState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    pub id: String,
    pub backend_id: String,
    pub labels: BTreeMap<String, String>,
    /// `None` when the daemon reports no expiry status, as an older one
    /// never does.
    pub expiry: Option<ExpiryStatus>,
    pub state: SandboxState,
    /// Why kilnd failed the sandbox, when it did; empty otherwise.
    pub failure_reason: String,
    /// The create's request ID, echoed; empty for an unkeyed create or a
    /// daemon without [`Feature::IdempotentCreate`].
    pub request_id: String,
    /// The create's request record was given up through a delete with
    /// `release_request_id`.
    pub request_id_released: bool,
}

/// What a successful `DeleteSandbox` reported beyond the deleted state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Deleted {
    /// kilnd gave up the create's request record in this call or an
    /// earlier one.
    pub request_id_released: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxExec {
    pub sandbox_id: String,
    pub argv: Vec<String>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
    /// The working directory, absolute; empty is kilnd's default, `/`.
    /// Needs [`Feature::ExecCwd`].
    pub cwd: String,
    /// Environment overrides, each name once. kilnd stores and hashes the
    /// values, so this never carries a secret. Needs [`Feature::ExecEnv`].
    pub env: Vec<EnvVar>,
}

/// One environment variable of an Exec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionState {
    Exited,
    TimedOut,
    Canceled,
    Failed,
    Interrupted,
    Unknown,
}

/// How kilnd says an execution was requested.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Recorded before kilnd had `ExecStream`, or a value this schema does
    /// not name.
    #[default]
    Unspecified,
    /// A unary `Exec`.
    Unary,
    /// An `ExecStream`.
    Stream,
}

impl ExecutionMode {
    fn from_proto(value: i32) -> Self {
        match pb::ExecutionMode::try_from(value) {
            Ok(pb::ExecutionMode::Unary) => Self::Unary,
            Ok(pb::ExecutionMode::Stream) => Self::Stream,
            Ok(pb::ExecutionMode::Unspecified) | Err(_) => Self::Unspecified,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapturedOutput {
    pub data: Vec<u8>,
    pub observed_bytes: u64,
    pub total_bytes_known: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    pub id: String,
    pub state: ExecutionState,
    pub exit_code: Option<i32>,
    pub failure_reason: String,
    pub stdout: CapturedOutput,
    pub stderr: CapturedOutput,
    /// `Some(false)`: the sandbox cannot take another call, even though
    /// this RPC succeeded. `None` when unknown, as from an older daemon.
    pub sandbox_reusable: Option<bool>,
    /// Why a `Canceled` execution was canceled, when kilnd says.
    pub cancel_reason: Option<CancelReason>,
    /// The working directory kilnd recorded, as it was sent; empty when
    /// none was.
    pub cwd: String,
    /// The names of the effective env overrides, sorted by name bytes.
    pub env_names: Vec<String>,
    /// kilnd's [`env_sha256`] of the effective overrides; empty when there
    /// are none.
    pub env_sha256: String,
    /// Whether it was requested through `Exec` or `ExecStream`.
    pub mode: ExecutionMode,
}

/// One output stream of an execution, as `ExecStream` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LiveStream {
    Stdout,
    Stderr,
}

impl LiveStream {
    /// `stdout` or `stderr`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }

    fn from_proto(value: i32) -> Option<Self> {
        match pb::OutputStream::try_from(value) {
            Ok(pb::OutputStream::Stdout) => Some(Self::Stdout),
            Ok(pb::OutputStream::Stderr) => Some(Self::Stderr),
            Ok(pb::OutputStream::Unspecified) | Err(_) => None,
        }
    }
}

/// Receives one `ExecStream`'s events in order, as they arrive. Bytes are
/// untrusted sandbox output.
pub trait StreamObserver {
    /// kilnd recorded the execution and started it.
    fn started(&mut self, execution_id: &str);
    /// Live bytes at `offset` of `stream`. `Break` drops the stream at once,
    /// which makes kilnd cancel the execution.
    fn chunk(&mut self, stream: LiveStream, offset: u64, data: &[u8]) -> ControlFlow<()>;
    /// `dropped` bytes from `offset` were not delivered live (`known ==
    /// false`: a lower bound). `Break` drops the stream as for a chunk.
    fn gap(
        &mut self,
        stream: LiveStream,
        offset: u64,
        dropped: u64,
        known: bool,
    ) -> ControlFlow<()>;
}

/// How much of one stream reached the client live.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveDelivery {
    pub delivered_bytes: u64,
    pub dropped_bytes: u64,
    /// `false`: `dropped_bytes` is a lower bound.
    pub dropped_bytes_known: bool,
}

impl LiveDelivery {
    fn from_proto(delivery: Option<pb::LiveDelivery>) -> Self {
        delivery.map_or_else(Self::default, |delivery| Self {
            delivered_bytes: delivery.delivered_bytes,
            dropped_bytes: delivery.dropped_bytes,
            dropped_bytes_known: delivery.dropped_bytes_known,
        })
    }
}

/// How an `ExecStream` that kilnd admitted ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamedExec {
    /// `ExecFinished` arrived: the stored row and live-delivery totals.
    Finished {
        execution: Execution,
        stdout: LiveDelivery,
        stderr: LiveDelivery,
    },
    /// The observer broke off after `ExecStarted`; the stream was dropped,
    /// so kilnd cancels the execution unless it had already finished. The
    /// caller reads the final row itself.
    Stopped { execution_id: String },
    /// The stream ended without `ExecFinished`: a transport error, a daemon
    /// restart, a clean end too early, or a protocol violation, after which
    /// the stream was dropped. `execution_id` is set when `ExecStarted`
    /// arrived. The execution may still be running, or never have started.
    Broken {
        execution_id: Option<String>,
        error: ApiError,
        /// kilnd broke `ExecStream`'s rules (order, offsets, totals, IDs),
        /// rather than the stream failing: what it sent cannot be trusted.
        protocol_violation: bool,
    },
}

/// What one `GetSandbox` says about whether the sandbox stayed usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalCheck {
    /// The sandbox is `READY`.
    pub ready: bool,
    /// Why it is being or was deleted, when it is: kilnd's deletion reason
    /// as the expiry it proves. `None` while live, or from an older daemon.
    pub deletion: Option<ExpiryReason>,
    /// Its most recent executions, newest first.
    pub recent: Vec<Execution>,
}

/// Why kilnd canceled an execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    ClientDisconnected,
    SandboxDeleted,
    DaemonShutdown,
    /// The sandbox's maximum lifetime passed while it ran.
    SandboxExpiredLifetime,
    /// A reason this schema does not name, by its protobuf number.
    Other(i32),
}

impl CancelReason {
    fn from_proto(value: i32) -> Option<Self> {
        Some(match pb::CancelReason::try_from(value) {
            Ok(pb::CancelReason::Unspecified) => return None,
            Ok(pb::CancelReason::ClientDisconnected) => Self::ClientDisconnected,
            Ok(pb::CancelReason::SandboxDeleted) => Self::SandboxDeleted,
            Ok(pb::CancelReason::DaemonShutdown) => Self::DaemonShutdown,
            Ok(pb::CancelReason::SandboxExpiredLifetime) => Self::SandboxExpiredLifetime,
            Err(_) => Self::Other(value),
        })
    }
}

impl Execution {
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.state == ExecutionState::Exited && self.exit_code == Some(0)
    }

    /// Whether kilnd recorded exactly this working directory and these env
    /// overrides for the execution. The harness sends no sandbox default
    /// env, so the effective overrides are the ones the Exec sent.
    #[must_use]
    pub fn ran_with(&self, cwd: &str, env: &[EnvVar]) -> bool {
        self.cwd == cwd && self.env_names == env_names(env) && self.env_sha256 == env_sha256(env)
    }
}

/// The env names kilnd records for an execution: sorted by name bytes, no
/// values.
#[must_use]
pub fn env_names(env: &[EnvVar]) -> Vec<String> {
    let mut names: Vec<String> = env.iter().map(|var| var.name.clone()).collect();
    names.sort_unstable();
    names
}

/// `Execution.env_sha256` for these overrides: lowercase hex SHA-256 over
/// `"kilnd.v1.exec-env.1"`, one zero byte, then each entry sorted by name
/// bytes as u32be length, name, u32be length, value. Empty for no entries.
#[must_use]
pub fn env_sha256(env: &[EnvVar]) -> String {
    use sha2::{Digest as _, Sha256};
    if env.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&EnvVar> = env.iter().collect();
    sorted.sort_unstable_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let mut hash = Sha256::new();
    hash.update(b"kilnd.v1.exec-env.1");
    hash.update([0]);
    // kilnd bounds names to 128 and values to 4096 bytes, far under u32.
    let length = |bytes: &str| u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes();
    for var in sorted {
        hash.update(length(&var.name));
        hash.update(var.name.as_bytes());
        hash.update(length(&var.value));
        hash.update(var.value.as_bytes());
    }
    hash.finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeReservations {
    pub sandboxes: u64,
    pub memory_bytes: u64,
    pub pids: u64,
    pub disk_bytes: u64,
}

/// An optional part of `kilnd.v1`. A client uses one only when a `SERVING`
/// Health response lists it: an older daemon ignores unknown request fields
/// silently and lists none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    SandboxExpiry,
    TypedReasons,
    IdempotentCreate,
    ExecCwd,
    ExecEnv,
    SandboxDefaultEnv,
    ExecStream,
    RequestIdRelease,
    /// A feature this harness does not use, by its protobuf number.
    Other(i32),
}

impl Feature {
    fn from_proto(value: i32) -> Option<Self> {
        Some(match pb::Feature::try_from(value) {
            Ok(pb::Feature::Unspecified) => return None,
            Ok(pb::Feature::SandboxExpiry) => Self::SandboxExpiry,
            Ok(pb::Feature::TypedReasons) => Self::TypedReasons,
            Ok(pb::Feature::IdempotentCreate) => Self::IdempotentCreate,
            Ok(pb::Feature::ExecCwd) => Self::ExecCwd,
            Ok(pb::Feature::ExecEnv) => Self::ExecEnv,
            Ok(pb::Feature::SandboxDefaultEnv) => Self::SandboxDefaultEnv,
            Ok(pb::Feature::ExecStream) => Self::ExecStream,
            Ok(pb::Feature::RequestIdRelease) => Self::RequestIdRelease,
            Err(_) => Self::Other(value),
        })
    }

    /// The name after `FEATURE_`, in lower case, as kilnd's reason tokens
    /// spell it; `feature_<n>` for a number this schema does not name.
    #[must_use]
    pub fn name(self) -> String {
        let named = match self {
            Self::SandboxExpiry => pb::Feature::SandboxExpiry,
            Self::TypedReasons => pb::Feature::TypedReasons,
            Self::IdempotentCreate => pb::Feature::IdempotentCreate,
            Self::ExecCwd => pb::Feature::ExecCwd,
            Self::ExecEnv => pb::Feature::ExecEnv,
            Self::SandboxDefaultEnv => pb::Feature::SandboxDefaultEnv,
            Self::ExecStream => pb::Feature::ExecStream,
            Self::RequestIdRelease => pb::Feature::RequestIdRelease,
            Self::Other(value) => match pb::Feature::try_from(value) {
                Ok(feature) => feature,
                Err(_) => return format!("feature_{value}"),
            },
        };
        let name = named.as_str_name();
        name.strip_prefix("FEATURE_")
            .unwrap_or(name)
            .to_ascii_lowercase()
    }
}

/// The daemon's expiry bounds, where each `max_*` of 0 means no cap, and
/// its exec-option bounds, which kilnd fills only while it lists the
/// feature (zero otherwise).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApiLimits {
    pub min_sandbox_lifetime_ms: u64,
    pub max_sandbox_lifetime_ms: u64,
    /// Used by kilnd only for a zero field of a sent policy; 0 never expires.
    pub default_sandbox_lifetime_ms: u64,
    pub min_sandbox_idle_ms: u64,
    pub max_sandbox_idle_ms: u64,
    pub default_sandbox_idle_ms: u64,
    /// Longest `ExecRequest.cwd` (`FEATURE_EXEC_CWD`).
    pub max_cwd_bytes: u32,
    /// Most `ExecRequest.env` entries (`FEATURE_EXEC_ENV`).
    pub max_env_entries: u32,
    pub max_env_name_bytes: u32,
    pub max_env_value_bytes: u32,
    /// Most bytes of the env, each entry counted as name + value + 2.
    pub max_env_total_bytes: u32,
    /// The timeout kilnd gives an `ExecStream` that sends none
    /// (`FEATURE_EXEC_STREAM`).
    pub default_stream_timeout_ms: u32,
    /// The longest `ExecStream` timeout kilnd accepts.
    pub max_stream_timeout_ms: u32,
}

/// What a daemon supports. An older daemon reports no features and no
/// limits, which reads as "unsupported" everywhere.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub features: BTreeSet<Feature>,
    pub limits: Option<ApiLimits>,
}

impl Capabilities {
    #[must_use]
    pub fn supports(&self, feature: Feature) -> bool {
        self.features.contains(&feature)
    }

    /// Every listed feature's [`Feature::name`], in order.
    #[must_use]
    pub fn feature_names(&self) -> Vec<String> {
        self.features.iter().map(|feature| feature.name()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub serving: bool,
    pub detail: String,
    pub active_sandboxes: u32,
    pub running_executions: u32,
    pub reservations: NodeReservations,
    /// Meaningful only when `serving`: a failed backend probe withdraws
    /// every dynamic feature.
    pub capabilities: Capabilities,
}

/// The metadata key of kilnd's typed refusal reasons.
pub const REASON_KEY: &str = "kilnd-reason";
/// Longest `kilnd-reason` token accepted.
pub const MAX_REASON_BYTES: usize = 128;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("kilnd {code}: {message}")]
pub struct ApiError {
    pub code: tonic::Code,
    pub message: String,
    /// The `kilnd-reason` trailer when it is a well-formed token: 1 to 128
    /// bytes of `[a-z0-9_:]`. It means something only when the daemon lists
    /// [`Feature::TypedReasons`]; the message prose is never parsed.
    pub reason: Option<String>,
}

impl ApiError {
    #[must_use]
    pub fn new(code: tonic::Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            reason: None,
        }
    }

    fn local(message: impl Into<String>) -> Self {
        // Local transport/protocol failures cannot prove create rejection.
        Self::new(tonic::Code::Unknown, message)
    }

    /// The reason token, if the session's daemon lists typed reasons.
    #[must_use]
    pub fn typed_reason(&self, typed_reasons: bool) -> Option<&str> {
        if typed_reasons {
            self.reason.as_deref()
        } else {
            None
        }
    }
}

/// Whether `token` is a reason the harness will act on.
#[must_use]
pub fn reason_is_valid(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_REASON_BYTES
        && token
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b':'))
}

impl From<tonic::Status> for ApiError {
    fn from(status: tonic::Status) -> Self {
        let reason = status
            .metadata()
            .get(REASON_KEY)
            .and_then(|value| value.to_str().ok())
            .filter(|token| reason_is_valid(token))
            .map(str::to_string);
        Self {
            code: status.code(),
            message: status.message().to_string(),
            reason,
        }
    }
}

/// Why a sandbox stopped being usable before the session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiryReason {
    /// kilnd reclaimed it after its idle timeout.
    ExpiredIdle,
    /// kilnd reclaimed it at its maximum lifetime.
    ExpiredLifetime,
    /// Someone deleted it.
    SandboxDeleted,
    /// The harness could not renew it for long enough that kilnd has
    /// probably reclaimed it.
    RenewFailed,
}

impl ExpiryReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExpiredIdle => "expired_idle",
            Self::ExpiredLifetime => "expired_lifetime",
            Self::SandboxDeleted => "sandbox_deleted",
            Self::RenewFailed => "renew_failed",
        }
    }

    /// The reason a sandbox RPC refusal proves the sandbox is gone:
    /// `FAILED_PRECONDITION` with an `expired_idle`, `expired_lifetime` or
    /// `sandbox_deleted` trailer, read only when the daemon lists typed
    /// reasons.
    #[must_use]
    pub fn of(error: &ApiError, typed_reasons: bool) -> Option<Self> {
        if error.code != tonic::Code::FailedPrecondition {
            return None;
        }
        match error.typed_reason(typed_reasons)? {
            "expired_idle" => Some(Self::ExpiredIdle),
            "expired_lifetime" => Some(Self::ExpiredLifetime),
            "sandbox_deleted" => Some(Self::SandboxDeleted),
            _ => None,
        }
    }
}

/// Why a refusal proves the sandbox is not ready and will not become ready
/// for this session, though it was not expired or deleted: kilnd's typed
/// "is X, not ready" refusals. The session's sandbox was ready when it was
/// created, so even `sandbox_not_ready` cannot be waited out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnusableReason {
    NotReady,
    Failed,
    Lost,
    Quarantined,
}

impl UnusableReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotReady => "sandbox_not_ready",
            Self::Failed => "sandbox_failed",
            Self::Lost => "sandbox_lost",
            Self::Quarantined => "sandbox_quarantined",
        }
    }

    /// `FAILED_PRECONDITION` with one of those tokens, read only when the
    /// daemon lists typed reasons.
    #[must_use]
    pub fn of(error: &ApiError, typed_reasons: bool) -> Option<Self> {
        if error.code != tonic::Code::FailedPrecondition {
            return None;
        }
        match error.typed_reason(typed_reasons)? {
            "sandbox_not_ready" => Some(Self::NotReady),
            "sandbox_failed" => Some(Self::Failed),
            "sandbox_lost" => Some(Self::Lost),
            "sandbox_quarantined" => Some(Self::Quarantined),
            _ => None,
        }
    }
}

/// The feature a typed `backend_unsupported:<feature>` reason names, when
/// it names one: `[a-z0-9_]+`, as `Feature` suffixes are spelled.
#[must_use]
pub fn backend_unsupported_feature(error: &ApiError, typed_reasons: bool) -> Option<&str> {
    if error.code != tonic::Code::Unimplemented {
        return None;
    }
    error
        .typed_reason(typed_reasons)?
        .strip_prefix("backend_unsupported:")
        .filter(|feature| {
            !feature.is_empty()
                && feature
                    .bytes()
                    .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_'))
        })
}

/// The exec option a typed `sandbox_unsupported:<option>` refusal names:
/// the sandbox's runtime did not prove `exec_cwd` or `exec_env`, and
/// nothing ran. Like every reason, read only when the daemon lists typed
/// reasons.
#[must_use]
pub fn sandbox_unsupported_exec_option(error: &ApiError, typed_reasons: bool) -> Option<&str> {
    if error.code != tonic::Code::FailedPrecondition {
        return None;
    }
    error
        .typed_reason(typed_reasons)?
        .strip_prefix("sandbox_unsupported:")
        .filter(|option| EXEC_OPTIONS.contains(option))
}

/// The exec options a typed unsupported refusal can name.
const EXEC_OPTIONS: [&str; 2] = ["exec_cwd", "exec_env"];

/// Whether kilnd refused an Exec because its backend or the sandbox's
/// runtime cannot apply the working directory or environment the harness
/// sends: typed `backend_unsupported:exec_cwd`/`:exec_env` or
/// `sandbox_unsupported:exec_cwd`/`:exec_env`. Nothing ran, and no later
/// Exec can succeed either.
#[must_use]
pub fn exec_option_unsupported(error: &ApiError, typed_reasons: bool) -> bool {
    backend_unsupported_feature(error, typed_reasons)
        .is_some_and(|feature| EXEC_OPTIONS.contains(&feature))
        || sandbox_unsupported_exec_option(error, typed_reasons).is_some()
}

/// Whether an Exec refusal proves this session's sandbox cannot take
/// another execution, though it was not expired or deleted: an
/// [`UnusableReason`], an unsupported exec option, or a backend that cannot
/// stream (`backend_unsupported:exec_stream`), since a session that streams
/// its model's commands never falls back to unary. Every boundary that
/// sends an Exec (seeding, the baseline, tools and the check) classifies
/// refusals with this one function.
#[must_use]
pub fn sandbox_unusable_refusal(error: &ApiError, typed_reasons: bool) -> bool {
    UnusableReason::of(error, typed_reasons).is_some()
        || exec_option_unsupported(error, typed_reasons)
        || backend_unsupported_feature(error, typed_reasons) == Some("exec_stream")
}

/// Whether an `ExecStream` refusal is kilnd's `exec_stream_limit`: the
/// node's or the caller's stream permits were all taken. kilnd refuses it
/// before anything is allocated, so the same request may be sent again
/// once a permit is free.
#[must_use]
pub fn exec_stream_limit(error: &ApiError, typed_reasons: bool) -> bool {
    error.code == tonic::Code::ResourceExhausted
        && error.typed_reason(typed_reasons) == Some("exec_stream_limit")
}

/// Whether an Exec refusal is kilnd's `execution_start_raced`: the sandbox
/// changed state while the execution was being recorded and is `READY`
/// again. Nothing ran, so the same request may be sent again.
#[must_use]
pub fn execution_start_raced(error: &ApiError, typed_reasons: bool) -> bool {
    error.code == tonic::Code::FailedPrecondition
        && error.typed_reason(typed_reasons) == Some("execution_start_raced")
}

impl std::fmt::Display for ExpiryReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[async_trait]
pub trait SandboxApi: Send + Sync {
    async fn health(&self) -> Result<Health, ApiError>;
    async fn create(&self, request: SandboxRequest) -> Result<Sandbox, ApiError>;
    async fn exec(&self, request: SandboxExec) -> Result<Execution, ApiError>;
    /// Run `request` through `ExecStream` (needs [`Feature::ExecStream`]).
    /// Events go to `observer` in order; when `observer` returns
    /// `ControlFlow::Break(())` the stream is dropped at once, which makes
    /// kilnd cancel the execution (`CLIENT_DISCONNECTED`).
    ///
    /// Any RPC error before the first message is a refusal before the row,
    /// returned as `Err` and classified exactly like an `exec` error.
    async fn exec_stream(
        &self,
        request: SandboxExec,
        observer: &mut (dyn StreamObserver + Send),
    ) -> Result<StreamedExec, ApiError>;
    async fn recent_executions(&self, sandbox_id: &str) -> Result<Vec<Execution>, ApiError>;
    /// Request idempotent deletion. Success acknowledges a deleted response state;
    /// callers must independently confirm absence via `non_deleted_sandboxes`.
    ///
    /// `release_request_id` asks kilnd to give up the create's request
    /// record (needs [`Feature::RequestIdRelease`]). It takes effect only if
    /// the row ends `DELETED`; otherwise nothing is released or remembered,
    /// so repeat the call once absence is confirmed. It is irreversible and
    /// ends retry protection for the key: pass `true` only after a
    /// successful keyed create whose response is held, with no create retry
    /// pending or to come.
    async fn delete(&self, sandbox_id: &str, release_request_id: bool)
    -> Result<Deleted, ApiError>;
    /// Record activity on a ready sandbox, restarting its idle timer; never
    /// extends its lifetime. Call only when the daemon lists
    /// [`Feature::SandboxExpiry`].
    async fn renew(&self, sandbox_id: &str) -> Result<(), ApiError>;
    /// Read one execution, as `GetExecution` reports it now.
    async fn get_execution(&self, execution_id: &str) -> Result<Execution, ApiError>;
    /// The sandbox's state and recent executions, in one `GetSandbox`, for
    /// the check that gates a completed session.
    async fn final_check(&self, sandbox_id: &str) -> Result<FinalCheck, ApiError>;
    /// Independently list every sandbox not yet deleted, including creating,
    /// deleting, and failed sandboxes. Errors or incomplete pagination must not
    /// be returned as a successful empty listing.
    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError>;
}

#[derive(Clone)]
pub struct TonicKilnd {
    client: KilndServiceClient<Channel>,
}

impl TonicKilnd {
    /// Connect a client channel to kilnd's Unix socket.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint cannot be configured or connected.
    pub async fn connect(path: PathBuf) -> Result<Self, ApiError> {
        let shown = path.display().to_string();
        let channel = Endpoint::try_from("http://kilnd.invalid")
            .map_err(|error| ApiError::local(error.to_string()))?
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = path.clone();
                async move {
                    UnixStream::connect(path)
                        .await
                        .map(TokioIo::new)
                        .map_err(std::io::Error::other)
                }
            }))
            .await
            .map_err(|error| ApiError::local(format!("connect to {shown}: {error}")))?;
        Ok(Self {
            client: KilndServiceClient::new(channel).max_decoding_message_size(4 << 20),
        })
    }

    fn sandbox(message: Option<pb::Sandbox>) -> Result<Sandbox, ApiError> {
        let sandbox = message.ok_or_else(|| ApiError::local("response omitted sandbox"))?;
        Ok(sandbox_from_proto(sandbox))
    }

    /// Read one sandbox, a retained deleted one included.
    ///
    /// # Errors
    ///
    /// Returns the RPC's error, or a local one if the response omits the
    /// sandbox.
    pub async fn get_sandbox(&self, sandbox_id: &str) -> Result<Sandbox, ApiError> {
        let response = self
            .client
            .clone()
            .get_sandbox(pb::GetSandboxRequest {
                id: sandbox_id.to_string(),
                execution_limit: 1,
            })
            .await
            .map_err(ApiError::from)?
            .into_inner();
        Self::sandbox(response.sandbox)
    }
}

fn sandbox_from_proto(sandbox: pb::Sandbox) -> Sandbox {
    Sandbox {
        id: sandbox.id,
        backend_id: sandbox.backend_id,
        labels: sandbox
            .spec
            .map_or_else(BTreeMap::new, |spec| spec.labels.into_iter().collect()),
        expiry: sandbox.expiry.map(|status| ExpiryStatus {
            effective: status.effective.map(|policy| ExpiryPolicy {
                max_lifetime_ms: policy.max_lifetime_ms,
                idle_timeout_ms: policy.idle_timeout_ms,
            }),
            lifetime_expires_at_unix_ms: status.lifetime_expires_at_unix_ms,
            idle_expires_at_unix_ms: status.idle_expires_at_unix_ms,
            last_activity_at_unix_ms: status.last_activity_at_unix_ms,
            idle_paused: status.idle_paused,
        }),
        state: SandboxState::from_proto(sandbox.state),
        failure_reason: sandbox.failure_reason,
        request_id: sandbox.request_id,
        request_id_released: sandbox.request_id_released,
    }
}

/// The wire form of a create. Labels are an ordered map, so equal requests
/// encode to the same bytes and a retry is byte-identical to its first
/// attempt.
fn create_request_to_proto(request: SandboxRequest) -> pb::CreateSandboxRequest {
    pb::CreateSandboxRequest {
        spec: Some(pb::SandboxSpec {
            image: request.image,
            limits: Some(pb::ResourceLimits {
                memory_bytes: request.memory_bytes,
                cpu_millis: request.cpu_millis,
                pids: request.pids,
                disk_bytes: request.disk_bytes,
            }),
            network: Some(pb::NetworkPolicy {
                mode: pb::NetworkMode::None as i32,
                allow: Vec::new(),
            }),
            read_only_root: request.read_only_root,
            writable_paths: request.writable_paths,
            labels: request.labels.into_iter().collect(),
        }),
        expiry: request.expiry.map(|policy| pb::ExpiryPolicy {
            max_lifetime_ms: policy.max_lifetime_ms,
            idle_timeout_ms: policy.idle_timeout_ms,
        }),
        request_id: request.request_id.unwrap_or_default(),
        // The harness sets no sandbox-wide env: each Exec sends its own.
        default_env: Vec::new(),
    }
}

/// The wire form of an Exec.
fn exec_request_to_proto(request: SandboxExec) -> Result<pb::ExecRequest, ApiError> {
    let timeout_ms = u32::try_from(request.timeout.as_millis())
        .map_err(|_| ApiError::local("execution timeout exceeds kilnd uint32 limit"))?;
    Ok(pb::ExecRequest {
        sandbox_id: request.sandbox_id,
        argv: request.argv,
        stdin: request.stdin,
        timeout_ms,
        cwd: request.cwd,
        env: request
            .env
            .into_iter()
            .map(|var| pb::EnvVar {
                name: var.name,
                value: var.value,
            })
            .collect(),
    })
}

fn capabilities_from_proto(capabilities: Option<pb::RuntimeCapabilities>) -> Capabilities {
    let Some(capabilities) = capabilities else {
        return Capabilities::default();
    };
    Capabilities {
        features: capabilities
            .features
            .into_iter()
            .filter_map(Feature::from_proto)
            .collect(),
        limits: capabilities.limits.map(|limits| ApiLimits {
            min_sandbox_lifetime_ms: limits.min_sandbox_lifetime_ms,
            max_sandbox_lifetime_ms: limits.max_sandbox_lifetime_ms,
            default_sandbox_lifetime_ms: limits.default_sandbox_lifetime_ms,
            min_sandbox_idle_ms: limits.min_sandbox_idle_ms,
            max_sandbox_idle_ms: limits.max_sandbox_idle_ms,
            default_sandbox_idle_ms: limits.default_sandbox_idle_ms,
            max_cwd_bytes: limits.max_cwd_bytes,
            max_env_entries: limits.max_env_entries,
            max_env_name_bytes: limits.max_env_name_bytes,
            max_env_value_bytes: limits.max_env_value_bytes,
            max_env_total_bytes: limits.max_env_total_bytes,
            default_stream_timeout_ms: limits.default_stream_timeout_ms,
            max_stream_timeout_ms: limits.max_stream_timeout_ms,
        }),
    }
}

#[async_trait]
impl SandboxApi for TonicKilnd {
    async fn health(&self) -> Result<Health, ApiError> {
        let response = self
            .client
            .clone()
            .health(pb::HealthRequest {})
            .await
            .map_err(ApiError::from)?
            .into_inner();
        let resources = response.node_resources.unwrap_or_default();
        Ok(Health {
            serving: response.status == pb::health_response::Status::Serving as i32,
            detail: response.detail,
            active_sandboxes: response.active_sandboxes,
            running_executions: response.running_executions,
            reservations: NodeReservations {
                sandboxes: resources.reserved_sandboxes,
                memory_bytes: resources.reserved_memory_bytes,
                pids: resources.reserved_pids,
                disk_bytes: resources.reserved_disk_bytes,
            },
            capabilities: capabilities_from_proto(response.runtime_capabilities),
        })
    }

    async fn create(&self, request: SandboxRequest) -> Result<Sandbox, ApiError> {
        if !request.network_none {
            return Err(ApiError::local(
                "this milestone accepts only network-none sandboxes",
            ));
        }
        let response = self
            .client
            .clone()
            .create_sandbox(create_request_to_proto(request))
            .await
            .map_err(ApiError::from)?
            .into_inner();
        Self::sandbox(response.sandbox)
    }

    async fn exec(&self, request: SandboxExec) -> Result<Execution, ApiError> {
        let response = self
            .client
            .clone()
            .exec(exec_request_to_proto(request)?)
            .await
            .map_err(ApiError::from)?
            .into_inner();
        let execution = response
            .execution
            .ok_or_else(|| ApiError::local("response omitted execution"))?;
        Ok(execution_from_proto(execution))
    }

    async fn exec_stream(
        &self,
        request: SandboxExec,
        observer: &mut (dyn StreamObserver + Send),
    ) -> Result<StreamedExec, ApiError> {
        let request = pb::ExecStreamRequest {
            exec: Some(exec_request_to_proto(request)?),
        };
        let mut stream = self
            .client
            .clone()
            .exec_stream(request)
            .await
            .map_err(ApiError::from)?
            .into_inner();
        let mut protocol = StreamProtocol::default();
        let broken = |protocol: &StreamProtocol, error: ApiError, protocol_violation: bool| {
            Ok(StreamedExec::Broken {
                execution_id: protocol.execution_id.clone(),
                error,
                protocol_violation,
            })
        };
        let mut finished = None;
        // Returning drops `stream`, which is the disconnect kilnd cancels on.
        loop {
            let message = match stream.message().await {
                Ok(Some(message)) => message,
                Ok(None) => {
                    return match finished {
                        Some(finished) => Ok(finished),
                        None => broken(
                            &protocol,
                            ApiError::local("ExecStream ended without ExecFinished"),
                            false,
                        ),
                    };
                }
                Err(status) => {
                    return match finished {
                        // ExecFinished is the stored row; an error status
                        // after it is no message and changes nothing.
                        Some(finished) => Ok(finished),
                        // A refusal before the row: nothing ran, classified
                        // as for a unary Exec.
                        None if protocol.next_seq == 0 => Err(ApiError::from(status)),
                        None => broken(&protocol, ApiError::from(status), false),
                    };
                }
            };
            match protocol.accept(message, observer) {
                StreamStep::Continue => {}
                StreamStep::Stop(execution_id) => {
                    return Ok(StreamedExec::Stopped { execution_id });
                }
                StreamStep::Finished(done) => finished = Some(*done),
                StreamStep::Violation(message) => {
                    return broken(
                        &protocol,
                        ApiError::new(tonic::Code::Internal, message),
                        true,
                    );
                }
            }
        }
    }

    async fn recent_executions(&self, sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        Ok(self.final_check(sandbox_id).await?.recent)
    }

    async fn delete(
        &self,
        sandbox_id: &str,
        release_request_id: bool,
    ) -> Result<Deleted, ApiError> {
        let response = self
            .client
            .clone()
            .delete_sandbox(pb::DeleteSandboxRequest {
                id: sandbox_id.to_string(),
                release_request_id,
            })
            .await
            .map_err(ApiError::from)?
            .into_inner();
        let sandbox = response
            .sandbox
            .ok_or_else(|| ApiError::local("delete response omitted sandbox"))?;
        if sandbox.state != pb::SandboxState::Deleted as i32 {
            return Err(ApiError::local(format!(
                "delete returned non-deleted state {}",
                sandbox.state
            )));
        }
        Ok(Deleted {
            request_id_released: sandbox.request_id_released,
        })
    }

    async fn renew(&self, sandbox_id: &str) -> Result<(), ApiError> {
        let response = self
            .client
            .clone()
            .renew_sandbox(pb::RenewSandboxRequest {
                id: sandbox_id.to_string(),
            })
            .await
            .map_err(ApiError::from)?
            .into_inner();
        check_renewed(response.sandbox.as_ref(), sandbox_id)
    }

    async fn get_execution(&self, execution_id: &str) -> Result<Execution, ApiError> {
        let response = self
            .client
            .clone()
            .get_execution(pb::GetExecutionRequest {
                id: execution_id.to_string(),
            })
            .await
            .map_err(ApiError::from)?
            .into_inner();
        response
            .execution
            .map(execution_from_proto)
            .ok_or_else(|| ApiError::local("response omitted execution"))
    }

    async fn final_check(&self, sandbox_id: &str) -> Result<FinalCheck, ApiError> {
        let response = self
            .client
            .clone()
            .get_sandbox(pb::GetSandboxRequest {
                id: sandbox_id.to_string(),
                execution_limit: 100,
            })
            .await
            .map_err(ApiError::from)?
            .into_inner();
        final_check_from_proto(response, sandbox_id)
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        let mut page_token = String::new();
        let mut sandboxes = Vec::new();
        loop {
            let response = self
                .client
                .clone()
                .list_sandboxes(pb::ListSandboxesRequest {
                    include_deleted: false,
                    page_size: Some(0),
                    page_token,
                })
                .await
                .map_err(ApiError::from)?
                .into_inner();
            sandboxes.extend(response.sandboxes.into_iter().map(sandbox_from_proto));
            page_token = response.next_page_token;
            if page_token.is_empty() {
                break;
            }
        }
        Ok(sandboxes)
    }
}

/// A `GetSandbox` answer must name the sandbox asked about; anything else is
/// a protocol error, `INTERNAL`, never an empty or unknown answer.
fn final_check_from_proto(
    response: pb::GetSandboxResponse,
    sandbox_id: &str,
) -> Result<FinalCheck, ApiError> {
    let protocol = |message| Err(ApiError::new(tonic::Code::Internal, message));
    let Some(sandbox) = response.sandbox else {
        return protocol("sandbox response omitted the sandbox");
    };
    if sandbox.id != sandbox_id {
        return protocol("sandbox response named another sandbox");
    }
    let deletion = match pb::DeletionReason::try_from(sandbox.deletion_reason) {
        Ok(pb::DeletionReason::ExpiredIdle) => Some(ExpiryReason::ExpiredIdle),
        Ok(pb::DeletionReason::ExpiredLifetime) => Some(ExpiryReason::ExpiredLifetime),
        Ok(pb::DeletionReason::Requested) => Some(ExpiryReason::SandboxDeleted),
        Ok(pb::DeletionReason::Unspecified) | Err(_) => None,
    };
    Ok(FinalCheck {
        ready: sandbox.state == pb::SandboxState::Ready as i32,
        deletion,
        recent: response
            .recent_executions
            .into_iter()
            .map(execution_from_proto)
            .collect(),
    })
}

/// A successful renew must answer with the renewed sandbox, still `READY`.
/// Anything else is a protocol error: `INTERNAL`, which the heartbeat treats
/// as terminal rather than transient.
fn check_renewed(sandbox: Option<&pb::Sandbox>, sandbox_id: &str) -> Result<(), ApiError> {
    let protocol = |message| Err(ApiError::new(tonic::Code::Internal, message));
    match sandbox {
        None => protocol("renew response omitted sandbox"),
        Some(sandbox) if sandbox.id != sandbox_id => {
            protocol("renew response named another sandbox")
        }
        Some(sandbox) if sandbox.state != pb::SandboxState::Ready as i32 => {
            protocol("renew response reported a sandbox that is not ready")
        }
        Some(_) => Ok(()),
    }
}

fn output(output: Option<pb::Output>) -> CapturedOutput {
    output.map_or_else(CapturedOutput::default, |output| CapturedOutput {
        data: output.data,
        observed_bytes: output.total_bytes,
        total_bytes_known: output.total_bytes_known,
        truncated: output.truncated,
    })
}

fn execution_from_proto(execution: pb::Execution) -> Execution {
    Execution {
        id: execution.id,
        state: match pb::ExecutionState::try_from(execution.state) {
            Ok(pb::ExecutionState::Exited) => ExecutionState::Exited,
            Ok(pb::ExecutionState::TimedOut) => ExecutionState::TimedOut,
            Ok(pb::ExecutionState::Canceled) => ExecutionState::Canceled,
            Ok(pb::ExecutionState::Failed) => ExecutionState::Failed,
            Ok(pb::ExecutionState::Interrupted) => ExecutionState::Interrupted,
            Ok(pb::ExecutionState::Unspecified | pb::ExecutionState::Running) | Err(_) => {
                ExecutionState::Unknown
            }
        },
        exit_code: execution.exit_code,
        failure_reason: execution.failure_reason,
        stdout: output(execution.stdout),
        stderr: output(execution.stderr),
        sandbox_reusable: execution.sandbox_reusable,
        cancel_reason: CancelReason::from_proto(execution.cancel_reason),
        cwd: execution.cwd,
        env_names: execution.env_names,
        env_sha256: execution.env_sha256,
        mode: ExecutionMode::from_proto(execution.mode),
    }
}

/// One step of an `ExecStream` as [`StreamProtocol`] judged it.
#[derive(Debug)]
enum StreamStep {
    /// Keep reading.
    Continue,
    /// The observer broke off after `ExecStarted`.
    Stop(String),
    /// `ExecFinished`: the stream must end next.
    Finished(Box<StreamedExec>),
    /// kilnd broke the stream protocol.
    Violation(&'static str),
}

/// The client side of `ExecStream`'s ordering rules: `seq` starts at 0 and
/// grows by one; message 0 is `ExecStarted` and no other is; chunks and gaps
/// name stdout or stderr; `ExecFinished` names the started execution and is
/// the last message. Each output stream's offsets start at 0, and every
/// chunk and gap starts exactly where the stream's previous one ended, so
/// no byte is counted twice and none goes missing without a gap: kilnd's
/// emitter sends an interior gap right before the chunk it precedes and
/// tail gaps at the stream's end. `ExecFinished`'s live totals must equal
/// what the stream carried.
#[derive(Debug, Default)]
struct StreamProtocol {
    next_seq: u64,
    execution_id: Option<String>,
    finished: bool,
    /// Per output stream (stdout, stderr): where its next chunk or gap must
    /// start, the bytes its chunks carried, and the bytes its gaps
    /// reported dropped.
    next_offset: [u64; 2],
    delivered: [u64; 2],
    dropped: [u64; 2],
}

impl StreamProtocol {
    fn accept(
        &mut self,
        message: pb::ExecStreamResponse,
        observer: &mut (dyn StreamObserver + Send),
    ) -> StreamStep {
        use pb::exec_stream_response::Event;
        if self.finished {
            return StreamStep::Violation("ExecStream sent a message after ExecFinished");
        }
        if message.seq != self.next_seq {
            return StreamStep::Violation("ExecStream messages were not numbered in order");
        }
        self.next_seq = self.next_seq.saturating_add(1);
        let event = match (message.event, &self.execution_id) {
            (Some(Event::Started(started)), None) => {
                if started.execution_id.is_empty() {
                    return StreamStep::Violation("ExecStarted named no execution");
                }
                observer.started(&started.execution_id);
                self.execution_id = Some(started.execution_id);
                return StreamStep::Continue;
            }
            (_, None) => {
                return StreamStep::Violation("ExecStream did not start with ExecStarted");
            }
            (Some(Event::Started(_)), Some(_)) => {
                return StreamStep::Violation("ExecStream sent a second ExecStarted");
            }
            (None, Some(_)) => return StreamStep::Violation("ExecStream sent an empty message"),
            (Some(event), Some(_)) => event,
        };
        let flow = match event {
            Event::Started(_) => unreachable!("handled above"),
            Event::Chunk(chunk) => {
                let Some(stream) = LiveStream::from_proto(chunk.stream) else {
                    return StreamStep::Violation("OutputChunk named no output stream");
                };
                let length = u64::try_from(chunk.data.len()).unwrap_or(u64::MAX);
                if let Err(violation) = self.advance(stream, chunk.offset, length, false) {
                    return StreamStep::Violation(violation);
                }
                observer.chunk(stream, chunk.offset, &chunk.data)
            }
            Event::Gap(gap) => {
                let Some(stream) = LiveStream::from_proto(gap.stream) else {
                    return StreamStep::Violation("OutputGap named no output stream");
                };
                if let Err(violation) = self.advance(stream, gap.offset, gap.dropped_bytes, true) {
                    return StreamStep::Violation(violation);
                }
                observer.gap(
                    stream,
                    gap.offset,
                    gap.dropped_bytes,
                    gap.dropped_bytes_known,
                )
            }
            Event::Finished(finished) => {
                let Some(execution) = finished.execution else {
                    return StreamStep::Violation("ExecFinished omitted the execution");
                };
                if Some(&execution.id) != self.execution_id.as_ref() {
                    return StreamStep::Violation("ExecFinished named another execution");
                }
                let stdout = LiveDelivery::from_proto(finished.stdout);
                let stderr = LiveDelivery::from_proto(finished.stderr);
                let carried = |index: usize, delivery: &LiveDelivery| {
                    delivery.delivered_bytes == self.delivered[index]
                        && delivery.dropped_bytes == self.dropped[index]
                };
                if !carried(0, &stdout) || !carried(1, &stderr) {
                    return StreamStep::Violation(
                        "ExecFinished's live totals differ from what the stream carried",
                    );
                }
                self.finished = true;
                return StreamStep::Finished(Box::new(StreamedExec::Finished {
                    execution: execution_from_proto(execution),
                    stdout,
                    stderr,
                }));
            }
        };
        match flow {
            ControlFlow::Continue(()) => StreamStep::Continue,
            ControlFlow::Break(()) => {
                StreamStep::Stop(self.execution_id.clone().unwrap_or_default())
            }
        }
    }

    /// Accept `length` bytes of `stream` at `offset`, carried by a chunk or,
    /// with `gap`, reported dropped: they must start exactly where the
    /// stream's previous chunk or gap ended, and no count may overflow.
    fn advance(
        &mut self,
        stream: LiveStream,
        offset: u64,
        length: u64,
        gap: bool,
    ) -> Result<(), &'static str> {
        let index = match stream {
            LiveStream::Stdout => 0,
            LiveStream::Stderr => 1,
        };
        if offset != self.next_offset[index] {
            return Err(if gap {
                "OutputGap did not start where the stream's previous output ended"
            } else {
                "OutputChunk did not start where the stream's previous output ended"
            });
        }
        let total = if gap {
            &mut self.dropped[index]
        } else {
            &mut self.delivered[index]
        };
        let (Some(next), Some(counted)) = (offset.checked_add(length), total.checked_add(length))
        else {
            return Err("ExecStream offsets overflowed");
        };
        self.next_offset[index] = next;
        *total = counted;
        Ok(())
    }
}

#[cfg(test)]
mod status_contract_test {
    use super::{ApiError, ExpiryReason, Feature, REASON_KEY};
    use tonic::metadata::{MetadataMap, MetadataValue};
    use tonic::{Code, Status};

    fn with_reason(code: Code, reason: &'static str) -> Status {
        let mut metadata = MetadataMap::new();
        metadata.insert(REASON_KEY, MetadataValue::from_static(reason));
        Status::with_metadata(code, format!("{reason}: prose"), metadata)
    }

    #[test]
    fn reason_trailers_are_validated_tokens() {
        let error = ApiError::from(with_reason(Code::FailedPrecondition, "expired_idle"));
        assert_eq!(error.reason.as_deref(), Some("expired_idle"));
        assert_eq!(error.message, "expired_idle: prose");
        let error = ApiError::from(with_reason(
            Code::Unimplemented,
            "backend_unsupported:exec_env",
        ));
        assert_eq!(
            error.reason.as_deref(),
            Some("backend_unsupported:exec_env")
        );
        for bad in ["Expired_idle", "expired idle", "expired-idle", "", "\u{e9}"] {
            let mut metadata = MetadataMap::new();
            if let Ok(value) = MetadataValue::try_from(bad) {
                metadata.insert(REASON_KEY, value);
            }
            let status = Status::with_metadata(Code::FailedPrecondition, "x", metadata);
            assert_eq!(ApiError::from(status).reason, None, "{bad:?}");
        }
        let long = "a".repeat(129);
        let mut metadata = MetadataMap::new();
        metadata.insert(REASON_KEY, MetadataValue::try_from(&long).expect("ascii"));
        let status = Status::with_metadata(Code::FailedPrecondition, "x", metadata);
        assert_eq!(ApiError::from(status).reason, None);
        let exact = "a".repeat(128);
        let mut metadata = MetadataMap::new();
        metadata.insert(REASON_KEY, MetadataValue::try_from(&exact).expect("ascii"));
        let status = Status::with_metadata(Code::FailedPrecondition, "x", metadata);
        assert_eq!(ApiError::from(status).reason, Some(exact));
        // The prose prefix alone is never parsed.
        let error = ApiError::from(Status::failed_precondition("expired_idle: prose"));
        assert_eq!(error.reason, None);
    }

    #[test]
    fn expiry_needs_the_code_the_token_and_the_feature() {
        let error = ApiError::from(with_reason(Code::FailedPrecondition, "expired_lifetime"));
        assert_eq!(
            ExpiryReason::of(&error, true),
            Some(ExpiryReason::ExpiredLifetime)
        );
        assert_eq!(ExpiryReason::of(&error, false), None);
        let error = ApiError::from(with_reason(Code::NotFound, "expired_idle"));
        assert_eq!(ExpiryReason::of(&error, true), None);
        let error = ApiError::from(with_reason(Code::FailedPrecondition, "sandbox_lost"));
        assert_eq!(ExpiryReason::of(&error, true), None);
    }

    #[test]
    fn non_ready_reasons_are_unusable_only_as_typed_failed_preconditions() {
        use super::UnusableReason;
        for (token, reason) in [
            ("sandbox_not_ready", UnusableReason::NotReady),
            ("sandbox_failed", UnusableReason::Failed),
            ("sandbox_lost", UnusableReason::Lost),
            ("sandbox_quarantined", UnusableReason::Quarantined),
        ] {
            let error = ApiError::from(with_reason(Code::FailedPrecondition, token));
            assert_eq!(UnusableReason::of(&error, true), Some(reason));
            assert_eq!(reason.as_str(), token);
            assert_eq!(UnusableReason::of(&error, false), None);
            assert_eq!(ExpiryReason::of(&error, true), None);
            let error = ApiError::from(with_reason(Code::Unavailable, token));
            assert_eq!(UnusableReason::of(&error, true), None);
        }
        let error = ApiError::from(with_reason(Code::FailedPrecondition, "expired_idle"));
        assert_eq!(UnusableReason::of(&error, true), None);
    }

    #[test]
    fn backend_unsupported_must_name_a_feature() {
        use super::backend_unsupported_feature;
        let named = ApiError::from(with_reason(
            Code::Unimplemented,
            "backend_unsupported:exec_env",
        ));
        assert_eq!(backend_unsupported_feature(&named, true), Some("exec_env"));
        assert_eq!(backend_unsupported_feature(&named, false), None);
        for bad in [
            "backend_unsupported:",
            "backend_unsupported::",
            "backend_unsupported:exec:env",
            "backend_unsupported",
            "sandbox_unsupported:exec_env",
        ] {
            let error = ApiError::from(with_reason(Code::Unimplemented, bad));
            assert_eq!(backend_unsupported_feature(&error, true), None, "{bad}");
        }
        let wrong_code = ApiError::from(with_reason(
            Code::FailedPrecondition,
            "backend_unsupported:exec_env",
        ));
        assert_eq!(backend_unsupported_feature(&wrong_code, true), None);
    }

    #[test]
    fn a_sandbox_answer_must_name_the_sandbox() {
        use super::final_check_from_proto;
        use crate::pb;
        let deleted = pb::Sandbox {
            id: "sb-0000000000000001".to_string(),
            state: pb::SandboxState::Deleted as i32,
            deletion_reason: pb::DeletionReason::ExpiredIdle as i32,
            ..pb::Sandbox::default()
        };
        let check = final_check_from_proto(
            pb::GetSandboxResponse {
                sandbox: Some(deleted.clone()),
                recent_executions: Vec::new(),
            },
            "sb-0000000000000001",
        )
        .expect("named");
        assert!(!check.ready);
        assert_eq!(check.deletion, Some(ExpiryReason::ExpiredIdle));
        for response in [
            pb::GetSandboxResponse::default(),
            pb::GetSandboxResponse {
                sandbox: Some(deleted),
                recent_executions: Vec::new(),
            },
        ] {
            let error = final_check_from_proto(response, "sb-0000000000000002")
                .expect_err("omitted or another sandbox");
            assert_eq!(error.code, Code::Internal);
        }
    }

    #[test]
    fn a_renew_answer_must_be_the_ready_sandbox() {
        use super::check_renewed;
        use crate::pb;
        let ready = pb::Sandbox {
            id: "sb-0000000000000001".to_string(),
            state: pb::SandboxState::Ready as i32,
            ..pb::Sandbox::default()
        };
        assert!(check_renewed(Some(&ready), "sb-0000000000000001").is_ok());
        let deleting = pb::Sandbox {
            state: pb::SandboxState::Deleting as i32,
            ..ready.clone()
        };
        let error = check_renewed(Some(&deleting), "sb-0000000000000001").expect_err("not ready");
        assert_eq!(error.code, Code::Internal);
        let error = check_renewed(Some(&ready), "sb-0000000000000002").expect_err("other id");
        assert_eq!(error.code, Code::Internal);
        let error = check_renewed(None, "sb-0000000000000001").expect_err("omitted");
        assert_eq!(error.code, Code::Internal);
    }

    #[test]
    fn feature_names_are_lower_case_suffixes() {
        assert_eq!(Feature::SandboxExpiry.name(), "sandbox_expiry");
        assert_eq!(Feature::TypedReasons.name(), "typed_reasons");
        assert_eq!(Feature::RequestIdRelease.name(), "request_id_release");
        assert_eq!(Feature::IdempotentCreate.name(), "idempotent_create");
        assert_eq!(Feature::from_proto(8), Some(Feature::RequestIdRelease));
        assert_eq!(Feature::from_proto(2), Some(Feature::IdempotentCreate));
        assert_eq!(Feature::Other(99).name(), "feature_99");
        assert_eq!(Feature::from_proto(0), None);
        assert_eq!(Feature::from_proto(7), Some(Feature::TypedReasons));
        assert_eq!(Feature::from_proto(99), Some(Feature::Other(99)));
    }

    #[test]
    fn tonic_status_codes_survive_adapter_conversion() {
        for code in [
            Code::InvalidArgument,
            Code::ResourceExhausted,
            Code::FailedPrecondition,
            Code::PermissionDenied,
            Code::Unauthenticated,
        ] {
            let error = ApiError::from(Status::new(code, "rejected"));
            assert_eq!(error.code, code);
            assert_eq!(error.message, "rejected");
        }
        assert_eq!(ApiError::local("response lost").code, Code::Unknown);
    }

    #[test]
    fn request_ids_are_printable_ascii_of_one_to_sixty_four_bytes() {
        use super::request_id_is_valid;
        assert!(!request_id_is_valid(""));
        assert!(request_id_is_valid("a"));
        assert!(request_id_is_valid(&"a".repeat(64)));
        assert!(!request_id_is_valid(&"a".repeat(65)));
        assert!(request_id_is_valid("!~"));
        assert!(!request_id_is_valid("a b"));
        assert!(!request_id_is_valid("a\u{7f}"));
        assert!(!request_id_is_valid("a\tb"));
        assert!(!request_id_is_valid("caf\u{e9}"));
        // The harness's longest key before the year 10000: 12 hex digits of
        // Unix milliseconds, a 32-bit process ID and a 64-bit counter.
        let longest = format!(
            "kiln-harness:hs-{}-{}-{}",
            "f".repeat(12),
            "f".repeat(8),
            "f".repeat(16)
        );
        assert_eq!(longest.len(), 54);
        assert!(request_id_is_valid(&longest));
    }

    #[test]
    fn sandbox_states_map_both_ways() {
        use super::SandboxState;
        use crate::pb;
        for (proto, state, name) in [
            (
                pb::SandboxState::Creating,
                SandboxState::Creating,
                "CREATING",
            ),
            (pb::SandboxState::Ready, SandboxState::Ready, "READY"),
            (
                pb::SandboxState::Deleting,
                SandboxState::Deleting,
                "DELETING",
            ),
            (pb::SandboxState::Deleted, SandboxState::Deleted, "DELETED"),
            (pb::SandboxState::Failed, SandboxState::Failed, "FAILED"),
            (pb::SandboxState::Lost, SandboxState::Lost, "LOST"),
            (
                pb::SandboxState::Quarantined,
                SandboxState::Quarantined,
                "QUARANTINED",
            ),
        ] {
            assert_eq!(SandboxState::from_proto(proto as i32), state);
            assert_eq!(state.as_str(), name);
            assert_eq!(proto.as_str_name(), format!("SANDBOX_STATE_{name}"));
        }
        assert_eq!(SandboxState::from_proto(0), SandboxState::Other(0));
        assert_eq!(SandboxState::from_proto(99).as_str(), "STATE_99");
    }

    #[test]
    fn equal_create_requests_encode_to_the_same_bytes() {
        use super::{SandboxRequest, create_request_to_proto};
        use prost::Message as _;
        let keys = (0..16).map(|n| format!("label-{n:02}")).collect::<Vec<_>>();
        let request = |order: &mut dyn Iterator<Item = &String>| SandboxRequest {
            image: "alpine".to_string(),
            memory_bytes: 1,
            cpu_millis: 1,
            pids: 1,
            disk_bytes: 1,
            network_none: true,
            read_only_root: true,
            writable_paths: Vec::new(),
            labels: order.map(|key| (key.clone(), key.clone())).collect(),
            expiry: None,
            request_id: Some("kiln-harness:a".to_string()),
        };
        let forward = create_request_to_proto(request(&mut keys.iter()));
        let backward = create_request_to_proto(request(&mut keys.iter().rev()));
        assert_eq!(forward.request_id, "kiln-harness:a");
        assert_eq!(forward.encode_to_vec(), backward.encode_to_vec());
        let encoded = forward.encode_to_vec();
        for _ in 0..4 {
            assert_eq!(forward.clone().encode_to_vec(), encoded);
        }
    }

    #[test]
    fn an_exec_maps_its_cwd_and_env_in_order() {
        use super::{EnvVar, SandboxExec, exec_request_to_proto};
        use std::time::Duration;

        let var = |name: &str, value: &str| EnvVar {
            name: name.to_string(),
            value: value.to_string(),
        };
        let request = exec_request_to_proto(SandboxExec {
            sandbox_id: "sb-1".to_string(),
            argv: vec!["/bin/true".to_string()],
            stdin: b"in".to_vec(),
            timeout: Duration::from_millis(4_750),
            cwd: "/workspace".to_string(),
            env: vec![var("TERM", "dumb"), var("A", ""), var("PAGER", "cat")],
        })
        .expect("maps");
        assert_eq!(request.sandbox_id, "sb-1");
        assert_eq!(request.argv, ["/bin/true"]);
        assert_eq!(request.stdin, b"in");
        assert_eq!(request.timeout_ms, 4_750);
        assert_eq!(request.cwd, "/workspace");
        let env: Vec<_> = request
            .env
            .iter()
            .map(|var| (var.name.as_str(), var.value.as_str()))
            .collect();
        assert_eq!(env, [("TERM", "dumb"), ("A", ""), ("PAGER", "cat")]);

        let too_long = exec_request_to_proto(SandboxExec {
            sandbox_id: "sb-1".to_string(),
            argv: Vec::new(),
            stdin: Vec::new(),
            timeout: Duration::from_millis(u64::from(u32::MAX) + 1),
            cwd: String::new(),
            env: Vec::new(),
        })
        .expect_err("over uint32");
        assert_eq!(too_long.code, tonic::Code::Unknown);
    }

    #[test]
    fn exec_option_and_race_reasons_need_their_code_and_typed_reasons() {
        use super::{execution_start_raced, sandbox_unsupported_exec_option};
        let error = |code, reason: &str| ApiError {
            code,
            message: reason.to_string(),
            reason: Some(reason.to_string()),
        };
        let raced = error(tonic::Code::FailedPrecondition, "execution_start_raced");
        assert!(execution_start_raced(&raced, true));
        assert!(!execution_start_raced(&raced, false));
        assert!(!execution_start_raced(
            &error(tonic::Code::Aborted, "execution_start_raced"),
            true
        ));
        assert!(!execution_start_raced(
            &error(tonic::Code::FailedPrecondition, "sandbox_not_ready"),
            true
        ));
        // A race is not a readiness refusal.
        assert_eq!(super::UnusableReason::of(&raced, true), None);

        for option in ["exec_cwd", "exec_env"] {
            let refusal = error(
                tonic::Code::FailedPrecondition,
                &format!("sandbox_unsupported:{option}"),
            );
            assert_eq!(
                sandbox_unsupported_exec_option(&refusal, true),
                Some(option)
            );
            assert_eq!(sandbox_unsupported_exec_option(&refusal, false), None);
            let refusal = error(
                tonic::Code::Unimplemented,
                &format!("sandbox_unsupported:{option}"),
            );
            assert_eq!(sandbox_unsupported_exec_option(&refusal, true), None);
        }
        for other in ["sandbox_unsupported:exec_stream", "sandbox_unsupported:"] {
            let refusal = error(tonic::Code::FailedPrecondition, other);
            assert_eq!(
                sandbox_unsupported_exec_option(&refusal, true),
                None,
                "{other}"
            );
        }
    }

    #[test]
    fn env_digests_match_kilnds_goldens() {
        use super::{EnvVar, env_names, env_sha256};
        let env = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(name, value)| EnvVar {
                    name: (*name).to_string(),
                    value: (*value).to_string(),
                })
                .collect::<Vec<_>>()
        };
        // kilnd 0c79944 crates/kilnd/src/exec_env.rs, from Python's hashlib.
        assert_eq!(env_sha256(&[]), "");
        assert_eq!(
            env_sha256(&env(&[("A", "1")])),
            "2603f468ba3210c56637236b3bb4fa2c637b59c2bc4cab419908ebdeddf1938f"
        );
        assert_eq!(
            env_sha256(&env(&[("A", ""), ("B", "2")])),
            "c91d8149e5b845070bef2ab16f824e9cd307c6029b35347aa70412b8bea9de46"
        );
        assert_eq!(
            env_sha256(&env(&[("B", "2"), ("A", "")])),
            env_sha256(&env(&[("A", ""), ("B", "2")]))
        );
        assert_ne!(
            env_sha256(&env(&[("A", "B1")])),
            env_sha256(&env(&[("AB", "1")]))
        );
        // The harness's fixed overrides, computed the same way in hashlib.
        assert_eq!(
            env_sha256(&env(&[
                ("TERM", "dumb"),
                ("NO_COLOR", "1"),
                ("PAGER", "cat")
            ])),
            "2c496a2607f307502ab39276323adc62b701d61fc618bb3b82f23cdbeb4af041"
        );
        assert_eq!(env_names(&env(&[("b", "x"), ("A", "y")])), ["A", "b"]);
        assert_eq!(
            env_names(&env(&[
                ("TERM", "dumb"),
                ("NO_COLOR", "1"),
                ("PAGER", "cat")
            ])),
            ["NO_COLOR", "PAGER", "TERM"]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_stream_is_read_in_order_and_cut_when_the_observer_says() {
        use super::{
            ExecutionMode, LiveStream, StreamObserver, StreamProtocol, StreamStep, StreamedExec,
        };
        use crate::pb;
        use crate::pb::exec_stream_response::Event;
        use std::ops::ControlFlow;

        #[derive(Default)]
        struct Cut {
            limit: u64,
            seen: Vec<(LiveStream, u64, u64)>,
        }
        impl StreamObserver for Cut {
            fn started(&mut self, _execution_id: &str) {}
            fn chunk(&mut self, stream: LiveStream, offset: u64, data: &[u8]) -> ControlFlow<()> {
                self.gap(stream, offset, data.len() as u64, true)
            }
            fn gap(
                &mut self,
                stream: LiveStream,
                offset: u64,
                length: u64,
                _known: bool,
            ) -> ControlFlow<()> {
                self.seen.push((stream, offset, length));
                if offset + length > self.limit {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            }
        }
        let message = |seq, event| pb::ExecStreamResponse {
            seq,
            event: Some(event),
        };
        let started = Event::Started(pb::ExecStarted {
            execution_id: "ex-1".to_string(),
            ..pb::ExecStarted::default()
        });
        let chunk = |stream: pb::OutputStream, offset, data: &[u8]| {
            Event::Chunk(pb::OutputChunk {
                stream: stream as i32,
                offset,
                data: data.to_vec(),
            })
        };
        // The stream below carries two bytes of stderr live.
        let finished = Event::Finished(pb::ExecFinished {
            execution: Some(pb::Execution {
                id: "ex-1".to_string(),
                mode: pb::ExecutionMode::Stream as i32,
                ..pb::Execution::default()
            }),
            stderr: Some(pb::LiveDelivery {
                delivered_bytes: 2,
                dropped_bytes: 0,
                dropped_bytes_known: true,
            }),
            ..pb::ExecFinished::default()
        });

        let mut protocol = StreamProtocol::default();
        let mut observer = Cut {
            limit: 100,
            ..Cut::default()
        };
        assert!(matches!(
            protocol.accept(message(0, started.clone()), &mut observer),
            StreamStep::Continue
        ));
        assert!(matches!(
            protocol.accept(
                message(1, chunk(pb::OutputStream::Stderr, 0, b"ab")),
                &mut observer
            ),
            StreamStep::Continue
        ));
        let StreamStep::Finished(done) =
            protocol.accept(message(2, finished.clone()), &mut observer)
        else {
            panic!("finished");
        };
        assert!(matches!(*done, StreamedExec::Finished { ref execution, .. }
            if execution.mode == ExecutionMode::Stream));
        assert!(matches!(
            protocol.accept(
                message(3, chunk(pb::OutputStream::Stdout, 0, b"x")),
                &mut observer
            ),
            StreamStep::Violation(_)
        ));
        assert_eq!(observer.seen, [(LiveStream::Stderr, 0, 2)]);

        // Past the observer's limit: stop with the started ID.
        let mut protocol = StreamProtocol::default();
        let mut observer = Cut {
            limit: 4,
            ..Cut::default()
        };
        protocol.accept(message(0, started.clone()), &mut observer);
        assert!(matches!(
            protocol.accept(message(1, chunk(pb::OutputStream::Stdout, 0, b"yyyyy")), &mut observer),
            StreamStep::Stop(ref id) if id == "ex-1"
        ));

        // Out of order, unnamed or empty messages are violations.
        for (first, second) in [
            (message(1, started.clone()), None),
            (message(0, finished.clone()), None),
            (
                message(0, started.clone()),
                Some(message(1, started.clone())),
            ),
            (
                message(0, started.clone()),
                Some(message(1, chunk(pb::OutputStream::Unspecified, 0, b"x"))),
            ),
            (
                message(0, started.clone()),
                Some(pb::ExecStreamResponse {
                    seq: 1,
                    event: None,
                }),
            ),
            (message(0, Event::Started(pb::ExecStarted::default())), None),
        ] {
            let mut protocol = StreamProtocol::default();
            let mut observer = Cut {
                limit: 100,
                ..Cut::default()
            };
            let step = match second {
                None => protocol.accept(first, &mut observer),
                Some(second) => {
                    assert!(matches!(
                        protocol.accept(first, &mut observer),
                        StreamStep::Continue
                    ));
                    protocol.accept(second, &mut observer)
                }
            };
            assert!(matches!(step, StreamStep::Violation(_)), "{step:?}");
        }
    }

    #[test]
    fn execution_modes_map_from_the_wire() {
        use super::ExecutionMode;
        use crate::pb;
        assert_eq!(
            ExecutionMode::from_proto(pb::ExecutionMode::Unary as i32),
            ExecutionMode::Unary
        );
        assert_eq!(
            ExecutionMode::from_proto(pb::ExecutionMode::Stream as i32),
            ExecutionMode::Stream
        );
        assert_eq!(ExecutionMode::from_proto(0), ExecutionMode::Unspecified);
        assert_eq!(ExecutionMode::from_proto(99), ExecutionMode::Unspecified);
    }
}
