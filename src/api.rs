use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use hyper_util::rt::TokioIo;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    pub id: String,
    pub backend_id: String,
    pub labels: BTreeMap<String, String>,
    /// `None` when the daemon reports no expiry status, as an older one
    /// never does.
    pub expiry: Option<ExpiryStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxExec {
    pub sandbox_id: String,
    pub argv: Vec<String>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
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
            Ok(pb::Feature::RequestIdRelease) | Err(_) => Self::Other(value),
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

/// The daemon's expiry bounds. Each `max_*` of 0 means no cap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApiLimits {
    pub min_sandbox_lifetime_ms: u64,
    pub max_sandbox_lifetime_ms: u64,
    /// Used by kilnd only for a zero field of a sent policy; 0 never expires.
    pub default_sandbox_lifetime_ms: u64,
    pub min_sandbox_idle_ms: u64,
    pub max_sandbox_idle_ms: u64,
    pub default_sandbox_idle_ms: u64,
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
    async fn recent_executions(&self, sandbox_id: &str) -> Result<Vec<Execution>, ApiError>;
    /// Request idempotent deletion. Success acknowledges a deleted response state;
    /// callers must independently confirm absence via `non_deleted_sandboxes`.
    async fn delete(&self, sandbox_id: &str) -> Result<(), ApiError>;
    /// Record activity on a ready sandbox, restarting its idle timer; never
    /// extends its lifetime. Call only when the daemon lists
    /// [`Feature::SandboxExpiry`].
    async fn renew(&self, sandbox_id: &str) -> Result<(), ApiError>;
    /// Read one execution, as `GetExecution` reports it now.
    async fn get_execution(&self, execution_id: &str) -> Result<Execution, ApiError>;
    /// Whether the sandbox is `READY` now, by `GetSandbox`.
    async fn sandbox_ready(&self, sandbox_id: &str) -> Result<bool, ApiError>;
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
    }
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
            .create_sandbox(pb::CreateSandboxRequest {
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
            })
            .await
            .map_err(ApiError::from)?
            .into_inner();
        Self::sandbox(response.sandbox)
    }

    async fn exec(&self, request: SandboxExec) -> Result<Execution, ApiError> {
        let timeout_ms = u32::try_from(request.timeout.as_millis())
            .map_err(|_| ApiError::local("execution timeout exceeds kilnd uint32 limit"))?;
        let response = self
            .client
            .clone()
            .exec(pb::ExecRequest {
                sandbox_id: request.sandbox_id,
                argv: request.argv,
                stdin: request.stdin,
                timeout_ms,
            })
            .await
            .map_err(ApiError::from)?
            .into_inner();
        let execution = response
            .execution
            .ok_or_else(|| ApiError::local("response omitted execution"))?;
        Ok(execution_from_proto(execution))
    }

    async fn recent_executions(&self, sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        Ok(self.final_check(sandbox_id).await?.recent)
    }

    async fn delete(&self, sandbox_id: &str) -> Result<(), ApiError> {
        let response = self
            .client
            .clone()
            .delete_sandbox(pb::DeleteSandboxRequest {
                id: sandbox_id.to_string(),
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
        Ok(())
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

    async fn sandbox_ready(&self, sandbox_id: &str) -> Result<bool, ApiError> {
        Ok(self.final_check(sandbox_id).await?.ready)
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
        assert_eq!(Feature::Other(8).name(), "request_id_release");
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
}
