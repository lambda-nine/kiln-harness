//! A fake kilnd shared by host-side tests.
#![allow(dead_code)]

pub mod anthropic;
pub mod http;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kiln_harness::api::{
    ApiError, ApiLimits, CancelReason, Capabilities, CapturedOutput, Execution, ExecutionState,
    ExpiryStatus, Feature, Health, NodeReservations, Sandbox, SandboxApi, SandboxExec,
    SandboxRequest,
};
use kiln_harness::engine::HarnessConfig;
use tonic::Code;

#[derive(Clone, Default)]
pub struct FakeApi {
    pub state: Arc<Mutex<FakeState>>,
}

/// One scripted `RenewSandbox` answer.
pub enum RenewScript {
    Ok,
    Fail(ApiError),
    /// Expire the sandbox with this reason and refuse the renew, as kilnd
    /// does when expiry wins the race.
    Expire(&'static str),
}

#[derive(Default)]
pub enum DeleteBehavior {
    #[default]
    Remove,
    AcknowledgeOnly,
    FailAfterRemoving,
}

/// Test knobs; independent flags read better than a state enum here.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
pub struct FakeState {
    pub creates: Vec<SandboxRequest>,
    pub execs: Vec<SandboxExec>,
    pub executions: Vec<Execution>,
    pub deleted: Vec<String>,
    pub files: BTreeMap<String, Vec<u8>>,
    pub next_execution: u64,
    pub create_delay: Duration,
    pub create_ready_at: Option<std::time::Instant>,
    pub suppress_create_record: bool,
    pub create_error_after_record: bool,
    pub create_rejection: Option<ApiError>,
    pub tool_exec_delay: Duration,
    pub baseline_exec_delay: Duration,
    pub fail_baseline: bool,
    pub delete_failures: u32,
    pub delete_attempts: u32,
    pub delete_behavior: DeleteBehavior,
    pub delete_acknowledgements_before_removal: u32,
    pub list_failures: u32,
    pub list_delay: Duration,
    pub list_attempts: u32,
    /// Bytes `head` returns instead of the stored file, to simulate a sandbox
    /// that answers differently from a recording.
    pub read_override: Option<Vec<u8>>,
    /// Let delete succeed while a create is still in flight.
    pub create_allows_early_delete: bool,
    /// Record the created row this long after the request, independently of
    /// the RPC.
    pub create_record_delay: Duration,
    /// What Health lists; empty, like a daemon that predates discovery.
    pub features: BTreeSet<Feature>,
    pub limits: Option<ApiLimits>,
    pub not_serving: bool,
    pub health_calls: u32,
    /// Every `RenewSandbox` call, answered from `renew_script` and then
    /// with success (or the expiry refusal once expired).
    pub renews: u32,
    pub renew_script: VecDeque<RenewScript>,
    pub renew_delay: Duration,
    /// Set once the sandbox expired: Exec and Renew refuse with this
    /// `kilnd-reason`, and the row has left the default listing.
    pub expired: Option<&'static str>,
    /// The Nth Exec (from 1, the baseline included) succeeds as an RPC but
    /// reports a canceled execution that left the sandbox unusable, with
    /// this cause.
    pub unusable_exec: Option<(usize, Option<CancelReason>)>,
    /// Every non-baseline Exec is refused with this error after
    /// `tool_exec_delay`, before admission: no execution row is recorded.
    pub tool_exec_error: Option<ApiError>,
    /// The Nth Exec (from 1) records its execution row, then its response
    /// is lost: the RPC fails `UNAVAILABLE`.
    pub lose_exec_response: Option<usize>,
}

impl FakeApi {
    pub fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().expect("fake state lock")
    }

    /// A fake of kilnd 853dee2: sandbox expiry and typed reasons with its
    /// default bounds.
    pub fn with_expiry() -> Self {
        let api = Self::default();
        {
            let mut state = api.state();
            state.features = BTreeSet::from([Feature::SandboxExpiry, Feature::TypedReasons]);
            state.limits = Some(kilnd_limits());
        }
        api
    }

    /// The scripted refusal of a non-baseline Exec and its delay, recording
    /// the request but no execution row.
    fn pre_admission_refusal(&self, request: &SandboxExec) -> Option<(ApiError, Duration)> {
        let mut state = self.state();
        let baseline = request.argv.first().map(String::as_str) == Some("/usr/bin/find");
        let error = state.tool_exec_error.clone().filter(|_| !baseline)?;
        state.execs.push(request.clone());
        Some((error, state.tool_exec_delay))
    }

    /// Expire the sandbox now, as kilnd's sweeper would.
    pub fn expire(&self, reason: &'static str) {
        self.state().expired = Some(reason);
    }
}

/// kilnd 853dee2's default expiry bounds.
pub fn kilnd_limits() -> ApiLimits {
    ApiLimits {
        min_sandbox_lifetime_ms: 10_000,
        max_sandbox_lifetime_ms: 7 * 86_400_000,
        default_sandbox_lifetime_ms: 0,
        min_sandbox_idle_ms: 10_000,
        max_sandbox_idle_ms: 86_400_000,
        default_sandbox_idle_ms: 0,
    }
}

/// The refusal kilnd sends for a sandbox that is gone.
pub fn expired_error(reason: &'static str) -> ApiError {
    api_error_with_reason(
        Code::FailedPrecondition,
        reason,
        &format!("{reason}: sandbox sb-0000000000000001 is deleted, not ready"),
    )
}

#[async_trait]
impl SandboxApi for FakeApi {
    async fn health(&self) -> Result<Health, ApiError> {
        let mut state = self.state();
        state.health_calls += 1;
        Ok(Health {
            serving: !state.not_serving,
            detail: if state.not_serving {
                "shutting down".to_string()
            } else {
                String::new()
            },
            active_sandboxes: u32::from(state.creates.len() > state.deleted.len()),
            running_executions: 0,
            reservations: NodeReservations::default(),
            capabilities: Capabilities {
                features: state.features.clone(),
                limits: state.limits,
            },
        })
    }

    async fn create(&self, request: SandboxRequest) -> Result<Sandbox, ApiError> {
        let labels = request.labels.clone();
        let (delay, fail) = {
            let mut state = self.state();
            if let Some(error) = &state.create_rejection {
                return Err(error.clone());
            }
            let delay = state.create_delay;
            let fail = state.create_error_after_record;
            if !state.suppress_create_record {
                if state.create_record_delay.is_zero() {
                    state.creates.push(request);
                    state.create_ready_at = if state.create_allows_early_delete {
                        None
                    } else {
                        std::time::Instant::now().checked_add(delay)
                    };
                } else {
                    // The daemon records the row on its own schedule, even
                    // after the client drops the RPC.
                    let shared = self.state.clone();
                    let after = state.create_record_delay;
                    tokio::spawn(async move {
                        tokio::time::sleep(after).await;
                        shared
                            .lock()
                            .expect("fake state lock")
                            .creates
                            .push(request);
                    });
                }
            }
            (delay, fail)
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if fail {
            return Err(api_error(Code::Unavailable, "create response lost"));
        }
        Ok(Sandbox {
            id: "sb-0000000000000001".to_string(),
            backend_id: "test-backend-1".to_string(),
            labels,
            expiry: None,
        })
    }

    async fn exec(&self, request: SandboxExec) -> Result<Execution, ApiError> {
        let refusal = self.pre_admission_refusal(&request);
        if let Some((error, delay)) = refusal {
            // Refused before admission: no execution row and no activity.
            tokio::time::sleep(delay).await;
            return Err(error);
        }
        let (execution, delay, lost) = {
            let mut state = self.state();
            if let Some(reason) = state.expired {
                return Err(expired_error(reason));
            }
            state.next_execution += 1;
            let id = format!("ex-{:016x}", state.next_execution);
            state.execs.push(request.clone());
            let argv = &request.argv;
            let (exit_code, stdout, stderr) = match argv.first().map(String::as_str) {
                Some("/usr/bin/find") => {
                    if state.fail_baseline && state.execs.len() == 1 {
                        (1, Vec::new(), b"find failed".to_vec())
                    } else {
                        let mut output = Vec::new();
                        for path in state.files.keys() {
                            output.extend_from_slice(path.as_bytes());
                            output.push(0);
                        }
                        (0, output, Vec::new())
                    }
                }
                Some("/bin/dd") => {
                    let path = argv
                        .iter()
                        .find_map(|arg| arg.strip_prefix("of="))
                        .expect("dd output path")
                        .to_string();
                    state.files.insert(path, request.stdin.clone());
                    (0, Vec::new(), Vec::new())
                }
                Some("/bin/stat") => {
                    let path = argv.last().expect("stat path");
                    match state.files.get(path) {
                        Some(bytes) => (0, format!("{}\n", bytes.len()).into_bytes(), Vec::new()),
                        None => (1, Vec::new(), b"not found".to_vec()),
                    }
                }
                Some("/usr/bin/head") => {
                    let limit = argv[2].parse::<usize>().expect("head limit");
                    let path = argv.last().expect("head path");
                    let stored = state.read_override.as_ref().or(state.files.get(path));
                    match stored {
                        Some(bytes) => (0, bytes[..bytes.len().min(limit)].to_vec(), Vec::new()),
                        None => (1, Vec::new(), b"not found".to_vec()),
                    }
                }
                Some("/bin/ls") => {
                    let names: BTreeSet<_> = state
                        .files
                        .keys()
                        .filter_map(|path| path.strip_prefix("/workspace/"))
                        .collect();
                    let listing = names.into_iter().collect::<Vec<_>>().join("\n") + "\n";
                    (0, listing.into_bytes(), Vec::new())
                }
                Some("/usr/bin/wc") => {
                    let path = argv.last().expect("wc path");
                    let size = state.files.get(path).map_or(0, Vec::len);
                    (0, format!("{size} {path}\n").into_bytes(), Vec::new())
                }
                Some("/bin/big-output") => (0, vec![b'x'; 100 << 10], Vec::new()),
                _ => (127, Vec::new(), b"unsupported fake command".to_vec()),
            };
            let mut execution = Execution {
                id,
                state: ExecutionState::Exited,
                exit_code: Some(exit_code),
                failure_reason: String::new(),
                stdout: captured(stdout),
                stderr: captured(stderr),
                // An older daemon does not say; a current one does.
                sandbox_reusable: (!state.features.is_empty()).then_some(true),
                cancel_reason: None,
            };
            leave_unusable_if_scripted(&mut state, &mut execution);
            state.executions.push(execution.clone());
            let delay = if argv.first().map(String::as_str) == Some("/usr/bin/find") {
                state.baseline_exec_delay
            } else {
                state.tool_exec_delay
            };
            let lost = state.lose_exec_response == Some(state.execs.len());
            (execution, delay, lost)
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if lost {
            return Err(api_error(Code::Unavailable, "exec response lost"));
        }
        Ok(execution)
    }

    async fn delete(&self, sandbox_id: &str) -> Result<(), ApiError> {
        let mut state = self.state();
        state.delete_attempts += 1;
        if state
            .create_ready_at
            .is_some_and(|ready_at| std::time::Instant::now() < ready_at)
        {
            return Err(api_error(
                Code::FailedPrecondition,
                "create operation is still in progress",
            ));
        }
        if state.delete_failures > 0 {
            state.delete_failures -= 1;
            return Err(api_error(Code::Unavailable, "transient delete failure"));
        }
        if matches!(state.delete_behavior, DeleteBehavior::AcknowledgeOnly) {
            return Ok(());
        }
        if state.delete_acknowledgements_before_removal > 0 {
            state.delete_acknowledgements_before_removal -= 1;
            return Ok(());
        }
        if !state.deleted.iter().any(|id| id == sandbox_id) {
            state.deleted.push(sandbox_id.to_string());
        }
        if matches!(state.delete_behavior, DeleteBehavior::FailAfterRemoving) {
            return Err(api_error(Code::Unavailable, "delete response lost"));
        }
        Ok(())
    }

    async fn renew(&self, _sandbox_id: &str) -> Result<(), ApiError> {
        let delay = {
            let mut state = self.state();
            state.renews += 1;
            if let Some(reason) = state.expired {
                return Err(expired_error(reason));
            }
            match state.renew_script.pop_front() {
                Some(RenewScript::Fail(error)) => return Err(error),
                Some(RenewScript::Expire(reason)) => {
                    state.expired = Some(reason);
                    return Err(expired_error(reason));
                }
                Some(RenewScript::Ok) | None => {}
            }
            state.renew_delay
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        Ok(())
    }

    async fn recent_executions(&self, _sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        let state = self.state();
        Ok(state.executions.iter().rev().cloned().collect())
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        let delay = {
            let mut state = self.state();
            state.list_attempts += 1;
            if state.list_failures > 0 {
                state.list_failures -= 1;
                return Err(api_error(Code::Unavailable, "listing failed"));
            }
            state.list_delay
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let state = self.state();
        // An expired sandbox is deleted by kilnd itself and leaves the
        // default listing.
        Ok(
            if state.creates.len() > state.deleted.len() && state.expired.is_none() {
                vec![Sandbox {
                    id: "sb-0000000000000001".to_string(),
                    backend_id: "test-backend-1".to_string(),
                    labels: state
                        .creates
                        .last()
                        .map_or_else(BTreeMap::new, |request| request.labels.clone()),
                    expiry: state.creates.last().and_then(|request| {
                        request.expiry.map(|policy| ExpiryStatus {
                            effective: Some(policy),
                            ..ExpiryStatus::default()
                        })
                    }),
                }]
            } else {
                Vec::new()
            },
        )
    }
}

/// Apply `unusable_exec` to the execution just recorded.
fn leave_unusable_if_scripted(state: &mut FakeState, execution: &mut Execution) {
    if let Some((number, cause)) = state.unusable_exec
        && number == state.execs.len()
    {
        // The RPC succeeds, but the sandbox did not survive it.
        execution.state = ExecutionState::Canceled;
        execution.exit_code = None;
        execution.failure_reason = "sandbox is gone".to_string();
        execution.sandbox_reusable = Some(false);
        execution.cancel_reason = cause;
        state.expired = Some("sandbox_deleted");
    }
}

pub fn api_error(code: Code, message: &str) -> ApiError {
    ApiError::new(code, message)
}

/// An error carrying a `kilnd-reason` trailer, as the adapter maps one.
pub fn api_error_with_reason(code: Code, reason: &str, message: &str) -> ApiError {
    ApiError {
        reason: Some(reason.to_string()),
        ..ApiError::new(code, message)
    }
}

pub fn captured(data: Vec<u8>) -> CapturedOutput {
    CapturedOutput {
        observed_bytes: data.len() as u64,
        data,
        total_bytes_known: true,
        truncated: false,
    }
}

pub fn short_lifecycle_config() -> HarnessConfig {
    HarnessConfig {
        lifecycle_timeout: Duration::from_millis(100),
        lifecycle_retry_delay: Duration::from_millis(5),
        ..HarnessConfig::default()
    }
}

/// Encode `events` as a correctly chained journal, bypassing the writer's
/// grammar, so readers can be tested on journals no sink would write.
pub fn chained_lines(
    session_id: &str,
    events: Vec<kiln_harness::journal::JournalEvent>,
) -> Vec<String> {
    use sha2::{Digest, Sha256};
    let mut prev = kiln_harness::journal::GENESIS_SHA256.to_string();
    let mut lines = Vec::new();
    for (sequence, event) in events.into_iter().enumerate() {
        let record = kiln_harness::journal::JournalRecord {
            sequence: sequence as u64,
            unix_ms: 0,
            session_id: session_id.to_string(),
            prev_sha256: prev,
            event,
        };
        let line = serde_json::to_string(&record).expect("encode record");
        prev = Sha256::digest(line.as_bytes())
            .iter()
            .fold(String::new(), |mut hex, byte| {
                use std::fmt::Write as _;
                write!(hex, "{byte:02x}").expect("write to String");
                hex
            });
        lines.push(line);
    }
    lines
}

/// Write `lines` as a newline-terminated journal file.
pub fn write_journal_lines(path: &std::path::Path, lines: &[String]) {
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(path, text).expect("write journal");
}

/// A temporary directory with mode 0700 whatever the umask, so it passes the
/// journal's directory rules (a umask such as 0002 would make it
/// group-writable).
pub fn private_tempdir() -> std::io::Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Create `path` and any missing parents, setting every directory this
/// creates to `mode` explicitly instead of relying on the umask.
pub fn dir_all_with_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let mut missing = Vec::new();
    let mut current = path;
    while !current.exists() {
        missing.push(current.to_path_buf());
        current = current.parent().expect("an existing ancestor");
    }
    for directory in missing.iter().rev() {
        std::fs::create_dir(directory).expect("create directory");
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(mode))
            .expect("set directory mode");
    }
}
