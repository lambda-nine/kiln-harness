//! A fake kilnd shared by host-side tests.
#![allow(dead_code)]

pub mod anthropic;
pub mod http;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kiln_harness::api::{
    ApiError, ApiLimits, CancelReason, Capabilities, CapturedOutput, Deleted, Execution,
    ExecutionState, ExpiryReason, ExpiryStatus, Feature, FinalCheck, Health, NodeReservations,
    Sandbox, SandboxApi, SandboxExec, SandboxRequest, SandboxState,
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

/// One scripted `CreateSandbox` answer, taken in order, one per call.
#[derive(Debug, Clone)]
pub enum CreateScript {
    /// Refused before anything is recorded.
    Err(ApiError),
    /// The row is recorded (or found under its key), then the response is
    /// lost with this error.
    ErrAfterRecord(ApiError),
    /// Answered `OK` with the row settled in this state, with this failure
    /// reason. A `DELETED` row leaves the default listing.
    Settled(SandboxState, String),
    /// Answered `OK` and `READY`, echoing this request ID instead of the
    /// request's.
    EchoRequestId(String),
}

/// One scripted `Health` answer, taken in order, one per call; then the
/// fake answers from its own state.
#[derive(Debug, Clone)]
pub enum HealthScript {
    Err(ApiError),
    NotServing,
    /// Serving, listing exactly these features.
    Features(BTreeSet<Feature>),
    /// Answer from state, after this delay.
    Delay(Duration),
}

/// How the fake answers `/bin/sh -s`, given the script on stdin and the
/// fake's files, which it may change.
pub type ShHandler = Arc<dyn Fn(&[u8], &mut BTreeMap<String, Vec<u8>>) -> ShAnswer + Send + Sync>;

/// One `/bin/sh` execution's terminal state.
#[derive(Debug, Clone)]
pub struct ShAnswer {
    pub state: ExecutionState,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ShAnswer {
    pub fn exit(code: i32) -> Self {
        Self {
            state: ExecutionState::Exited,
            exit_code: Some(code),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    pub fn state(state: ExecutionState) -> Self {
        Self {
            state,
            exit_code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }
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
    /// The request of every row created; a keyed create answered from its
    /// record adds none.
    pub creates: Vec<SandboxRequest>,
    /// Every `CreateSandbox` request as sent, retries included, and when.
    pub create_requests: Vec<SandboxRequest>,
    pub create_sent_at: Vec<tokio::time::Instant>,
    pub create_script: VecDeque<CreateScript>,
    pub health_script: VecDeque<HealthScript>,
    /// Each `DeleteSandbox` call's `release_request_id`, and when.
    pub deletes_released: Vec<bool>,
    pub delete_sent_at: Vec<tokio::time::Instant>,
    /// Release-asking deletes of a removed keyed row that release nothing,
    /// as one that left the row `DELETING` would, before one releases.
    pub release_withheld: u32,
    /// Never release, as a daemon without request-ID release.
    pub release_refused: bool,
    /// The keyed row's request record was released.
    pub request_id_released: bool,
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
    /// The Nth Exec (from 1) is published RUNNING and its response lost;
    /// the row turns terminal only after this many `GetExecution` reads
    /// (`u32::MAX`: never).
    pub running_exec: Option<(usize, u32)>,
    /// The terminal form of a RUNNING row, with the reads left.
    pub pending_terminal: Option<(Execution, u32)>,
    /// `GetSandbox` reports the sandbox not `READY`, as after a refusal
    /// that was not a race.
    pub not_ready: bool,
    pub get_execution_calls: u32,
    /// The final check: how often it ran, an error to answer it with, and
    /// rows only it sees, as executions recovery missed.
    pub final_checks: u32,
    pub final_check_error: Option<ApiError>,
    pub final_rows: Vec<Execution>,
    /// Every recovery read of recent executions fails `UNAVAILABLE`.
    pub recent_unavailable: bool,
    /// Answers `/bin/sh`; without one it exits 127 like any unknown command.
    pub sh_handler: Option<ShHandler>,
    /// `/bin/sh` answers after this delay.
    pub sh_delay: Duration,
    /// `/bin/sh` is refused with this error, after `sh_delay`, with no row.
    pub sh_error: Option<ApiError>,
    /// `/bin/dd` exits 1 without writing, from the Nth `dd` on (from 1).
    pub fail_dd_from: Option<usize>,
    /// `dd` executions so far.
    pub dd_count: usize,
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

    /// A fake of kilnd with idempotent create, and, with `release`,
    /// request-ID release, beside expiry and typed reasons.
    pub fn keyed(release: bool) -> Self {
        let api = Self::with_expiry();
        {
            let mut state = api.state();
            state.features.insert(Feature::IdempotentCreate);
            if release {
                state.features.insert(Feature::RequestIdRelease);
            }
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
        let script = {
            let mut state = self.state();
            state.health_calls += 1;
            state.health_script.pop_front()
        };
        let mut features = None;
        let mut not_serving = false;
        match script {
            Some(HealthScript::Err(error)) => return Err(error),
            Some(HealthScript::NotServing) => not_serving = true,
            Some(HealthScript::Features(listed)) => features = Some(listed),
            Some(HealthScript::Delay(delay)) => tokio::time::sleep(delay).await,
            None => {}
        }
        let state = self.state();
        let not_serving = not_serving || state.not_serving;
        Ok(Health {
            serving: !not_serving,
            detail: if not_serving {
                "shutting down".to_string()
            } else {
                String::new()
            },
            active_sandboxes: u32::from(state.creates.len() > state.deleted.len()),
            running_executions: 0,
            reservations: NodeReservations::default(),
            capabilities: Capabilities {
                features: features.unwrap_or_else(|| state.features.clone()),
                limits: state.limits,
            },
        })
    }

    async fn create(&self, request: SandboxRequest) -> Result<Sandbox, ApiError> {
        let labels = request.labels.clone();
        let echoed = request.request_id.clone().unwrap_or_default();
        let (delay, fail, script, retained_deleted) = {
            let mut state = self.state();
            state.create_requests.push(request.clone());
            state.create_sent_at.push(tokio::time::Instant::now());
            let script = state.create_script.pop_front();
            if let Some(CreateScript::Err(error)) = script {
                return Err(error);
            }
            if let Some(error) = &state.create_rejection {
                return Err(error.clone());
            }
            let delay = state.create_delay;
            let fail = state.create_error_after_record;
            // kilnd's keyed lookup: an equal request under a recorded key is
            // answered from its row, an unequal one is a conflict. A released
            // record is gone: the key makes a new sandbox.
            let recorded = request
                .request_id
                .as_ref()
                .filter(|_| !state.request_id_released)
                .and_then(|key| {
                    state
                        .creates
                        .iter()
                        .find(|row| row.request_id.as_ref() == Some(key))
                });
            let retained_deleted =
                recorded.is_some() && state.deleted.iter().any(|id| id == "sb-0000000000000001");
            if let Some(recorded) = recorded {
                if recorded != &request {
                    return Err(api_error_with_reason(
                        Code::AlreadyExists,
                        "request_id_conflict",
                        "request_id_conflict: another request used this request ID",
                    ));
                }
            } else if !state.suppress_create_record {
                if state.create_record_delay.is_zero() {
                    // A later session's sandbox starts with an empty workspace.
                    if !state.creates.is_empty() {
                        state.files.clear();
                    }
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
                // A settled DELETED row is not listed.
                if let Some(CreateScript::Settled(SandboxState::Deleted, _)) = &script {
                    state.deleted.push("sb-0000000000000001".to_string());
                }
            }
            (delay, fail, script, retained_deleted)
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if fail {
            return Err(api_error(Code::Unavailable, "create response lost"));
        }
        let mut sandbox = Sandbox {
            id: "sb-0000000000000001".to_string(),
            backend_id: "test-backend-1".to_string(),
            labels,
            expiry: None,
            state: SandboxState::Ready,
            failure_reason: String::new(),
            request_id: echoed,
            request_id_released: false,
        };
        // A retained record whose sandbox is gone answers with the DELETED row.
        if retained_deleted {
            sandbox.state = SandboxState::Deleted;
        }
        match script {
            Some(CreateScript::ErrAfterRecord(error)) => return Err(error),
            Some(CreateScript::Settled(state, reason)) => {
                sandbox.state = state;
                sandbox.failure_reason = reason;
            }
            Some(CreateScript::EchoRequestId(request_id)) => sandbox.request_id = request_id,
            Some(CreateScript::Err(_)) | None => {}
        }
        Ok(sandbox)
    }

    #[allow(clippy::too_many_lines)]
    async fn exec(&self, request: SandboxExec) -> Result<Execution, ApiError> {
        let sh_refusal = {
            let mut state = self.state();
            if request.argv.first().map(String::as_str) == Some("/bin/sh") {
                state
                    .sh_error
                    .clone()
                    .map(|error| (error, state.sh_delay))
                    .inspect(|_| state.execs.push(request.clone()))
            } else {
                None
            }
        };
        if let Some((error, delay)) = sh_refusal {
            tokio::time::sleep(delay).await;
            return Err(error);
        }
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
            let mut sh_state = None;
            let (exit_code, stdout, stderr) = match argv.first().map(String::as_str) {
                Some("/bin/sh") => {
                    let handler = state.sh_handler.clone();
                    match handler {
                        Some(handler) => {
                            let answer = handler(&request.stdin, &mut state.files);
                            sh_state = Some((answer.state, answer.exit_code));
                            (answer.exit_code.unwrap_or(0), answer.stdout, answer.stderr)
                        }
                        None => (127, Vec::new(), b"no sh handler".to_vec()),
                    }
                }
                Some("/bin/mkdir") => (0, Vec::new(), Vec::new()),
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
                    state.dd_count += 1;
                    let path = argv
                        .iter()
                        .find_map(|arg| arg.strip_prefix("of="))
                        .expect("dd output path")
                        .to_string();
                    if state
                        .fail_dd_from
                        .is_some_and(|from| state.dd_count >= from)
                    {
                        (1, Vec::new(), b"dd: write error".to_vec())
                    } else {
                        state.files.insert(path, request.stdin.clone());
                        (0, Vec::new(), Vec::new())
                    }
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
            if let Some((exec_state, exec_code)) = sh_state {
                execution.state = exec_state;
                execution.exit_code = exec_code;
            }
            leave_unusable_if_scripted(&mut state, &mut execution);
            state.executions.push(execution.clone());
            let delay = if argv.first().map(String::as_str) == Some("/usr/bin/find") {
                state.baseline_exec_delay
            } else if argv.first().map(String::as_str) == Some("/bin/sh") {
                state.sh_delay
            } else {
                state.tool_exec_delay
            };
            let mut lost = state.lose_exec_response == Some(state.execs.len());
            lost |= publish_running_if_scripted(&mut state, &execution);
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

    async fn delete(
        &self,
        sandbox_id: &str,
        release_request_id: bool,
    ) -> Result<Deleted, ApiError> {
        let mut state = self.state();
        state.delete_attempts += 1;
        state.deletes_released.push(release_request_id);
        state.delete_sent_at.push(tokio::time::Instant::now());
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
        // A delete that leaves the row in place releases nothing.
        let unreleased = Deleted {
            request_id_released: state.request_id_released,
        };
        if matches!(state.delete_behavior, DeleteBehavior::AcknowledgeOnly) {
            return Ok(unreleased);
        }
        if state.delete_acknowledgements_before_removal > 0 {
            state.delete_acknowledgements_before_removal -= 1;
            return Ok(unreleased);
        }
        // One fake can serve sessions one after another: a delete counts
        // only while a created sandbox is still undeleted.
        if state.deleted.len() < state.creates.len() {
            state.deleted.push(sandbox_id.to_string());
        }
        let keyed = state
            .creates
            .last()
            .is_some_and(|request| request.request_id.is_some());
        if release_request_id && keyed && !state.release_refused {
            if state.release_withheld > 0 {
                state.release_withheld -= 1;
            } else {
                state.request_id_released = true;
            }
        }
        if matches!(state.delete_behavior, DeleteBehavior::FailAfterRemoving) {
            return Err(api_error(Code::Unavailable, "delete response lost"));
        }
        Ok(Deleted {
            request_id_released: state.request_id_released,
        })
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

    async fn get_execution(&self, execution_id: &str) -> Result<Execution, ApiError> {
        let mut state = self.state();
        state.get_execution_calls += 1;
        if let Some((terminal, reads)) = state.pending_terminal.take() {
            if terminal.id == execution_id && reads != u32::MAX && reads <= 1 {
                if let Some(row) = state
                    .executions
                    .iter_mut()
                    .find(|row| row.id == execution_id)
                {
                    *row = terminal;
                }
            } else if terminal.id == execution_id {
                let reads = if reads == u32::MAX { reads } else { reads - 1 };
                state.pending_terminal = Some((terminal, reads));
            } else {
                state.pending_terminal = Some((terminal, reads));
            }
        }
        state
            .executions
            .iter()
            .find(|row| row.id == execution_id)
            .cloned()
            .ok_or_else(|| api_error(Code::NotFound, "no such execution"))
    }

    async fn sandbox_ready(&self, _sandbox_id: &str) -> Result<bool, ApiError> {
        let state = self.state();
        Ok(
            state.expired.is_none()
                && !state.not_ready
                && state.creates.len() > state.deleted.len(),
        )
    }

    async fn final_check(&self, _sandbox_id: &str) -> Result<FinalCheck, ApiError> {
        let mut state = self.state();
        state.final_checks += 1;
        if let Some(error) = &state.final_check_error {
            return Err(error.clone());
        }
        let mut recent = state.final_rows.clone();
        recent.extend(state.executions.iter().rev().cloned());
        Ok(FinalCheck {
            ready: state.expired.is_none()
                && !state.not_ready
                && state.creates.len() > state.deleted.len(),
            deletion: state.expired.map(|reason| match reason {
                "expired_idle" => ExpiryReason::ExpiredIdle,
                "expired_lifetime" => ExpiryReason::ExpiredLifetime,
                _ => ExpiryReason::SandboxDeleted,
            }),
            recent,
        })
    }

    async fn recent_executions(&self, _sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        let state = self.state();
        if state.recent_unavailable {
            return Err(api_error(Code::Unavailable, "listing failed"));
        }
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
                    state: SandboxState::Ready,
                    failure_reason: String::new(),
                    request_id: state
                        .creates
                        .last()
                        .and_then(|request| request.request_id.clone())
                        .unwrap_or_default(),
                    request_id_released: false,
                }]
            } else {
                Vec::new()
            },
        )
    }
}

/// Apply `running_exec` to the execution just recorded: publish it RUNNING,
/// keep its terminal form for later reads, and report the response lost.
fn publish_running_if_scripted(state: &mut FakeState, execution: &Execution) -> bool {
    let Some((number, reads)) = state.running_exec else {
        return false;
    };
    if number != state.execs.len() {
        return false;
    }
    if let Some(row) = state.executions.last_mut() {
        *row = Execution {
            state: ExecutionState::Unknown,
            exit_code: None,
            ..execution.clone()
        };
    }
    state.pending_terminal = Some((execution.clone(), reads));
    true
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
