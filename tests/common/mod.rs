//! A fake kilnd shared by host-side tests.
#![allow(dead_code)]

pub mod anthropic;
pub mod http;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kiln_harness::api::{
    ApiError, ApiLimits, CancelReason, Capabilities, CapturedOutput, Deleted, EnvVar, Execution,
    ExecutionMode, ExecutionState, ExpiryReason, ExpiryStatus, Feature, FinalCheck, Health,
    LiveDelivery, LiveStream, NodeReservations, Reconnect, Sandbox, SandboxApi, SandboxExec,
    SandboxRequest, SandboxState, StreamObserver, StreamedExec, env_names, env_sha256,
};
use kiln_harness::engine::HarnessConfig;
use tonic::Code;

#[derive(Clone)]
pub struct FakeApi {
    pub state: Arc<Mutex<FakeState>>,
}

/// A fake of the oldest kilnd a live session accepts: it lists only the
/// exec options and exec streaming, with kilnd's default bounds for them.
impl Default for FakeApi {
    fn default() -> Self {
        let api = Self {
            state: Arc::default(),
        };
        api.state().features = live_features();
        api.state().limits = Some(exec_limits());
        api
    }
}

/// The exec options every session that sends a cwd and env needs.
pub fn exec_options() -> BTreeSet<Feature> {
    BTreeSet::from([Feature::ExecCwd, Feature::ExecEnv])
}

/// What a live session needs of every daemon: the exec options and exec
/// streaming.
pub fn live_features() -> BTreeSet<Feature> {
    let mut features = exec_options();
    features.insert(Feature::ExecStream);
    features
}

/// One scripted `ExecStream` event after `ExecStarted`.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Bytes of a stream at an offset.
    Chunk(LiveStream, u64, Vec<u8>),
    /// Bytes from an offset not delivered, and whether the count is exact.
    Gap(LiveStream, u64, u64, bool),
    /// The command runs on this long, quiet, before the next event.
    Sleep(Duration),
}

/// How a scripted `ExecStream` ends.
#[derive(Debug, Clone, Default)]
pub enum StreamEnd {
    /// `ExecFinished` with the recorded row.
    #[default]
    Finished,
    /// The stream breaks after its events.
    Broken(ApiError),
    /// The row is recorded, then the stream breaks before `ExecStarted`.
    BrokenBeforeStarted(ApiError),
    /// After its events, kilnd breaks the stream's rules, as the adapter
    /// reports it: `Broken` with an `INTERNAL` error, flagged a violation.
    Violation,
}

/// One `ExecStream` answer, taken in order, one per stream that kilnd
/// admits. `events: None` sends the row's retained stdout and stderr as one
/// chunk each.
#[derive(Debug, Clone, Default)]
pub struct StreamScript {
    pub events: Option<Vec<StreamEvent>>,
    pub end: StreamEnd,
}

/// Changes a recorded execution row, as a daemon that did not run it as
/// sent would report it.
pub type Tamper = fn(&mut Execution);

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

/// How the fake answers a `/bin/grep` its own emulation does not: given
/// the argv and the fake's files.
pub type GrepHandler = Arc<dyn Fn(&[String], &BTreeMap<String, Vec<u8>>) -> ShAnswer + Send + Sync>;

/// Changes the stdout capture of an execution, as a daemon that kept only
/// part of it would report it.
pub type CaptureTamper = fn(&mut CapturedOutput);

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
    /// What Health lists; `FakeApi::default` lists the exec options.
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
    /// Each non-baseline Exec takes the next of these, while any are left,
    /// and is refused with it like `tool_exec_error`.
    pub exec_refusals: VecDeque<ApiError>,
    /// Every baseline (`find`) Exec is refused with this error, before
    /// admission: no execution row is recorded.
    pub baseline_exec_error: Option<ApiError>,
    /// The Nth Exec (from 1) records its execution row, then its response
    /// is lost: the RPC fails `UNAVAILABLE`.
    pub lose_exec_response: Option<usize>,
    /// The Nth Exec's (from 1, the baseline included) row, as returned and
    /// as recovery reads it, is changed by this, as by a daemon that did
    /// not run it as sent.
    pub tamper_exec: Option<(usize, Tamper)>,
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
    /// The execution each `GetExecution` asked for, in order.
    pub get_execution_ids: Vec<String>,
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
    /// Every `ExecStream` request, refused ones included; each is also in
    /// `execs`, so every other entry there was a unary Exec.
    pub streams: Vec<SandboxExec>,
    /// Streams the client dropped before `ExecFinished`.
    pub streams_dropped: u32,
    /// How each admitted stream goes; the default once these run out.
    pub stream_script: VecDeque<StreamScript>,
    /// A dropped stream's row stays RUNNING for this many `GetExecution`
    /// reads (`u32::MAX`: for ever) before it settles; `None` settles it at
    /// once.
    pub stopped_settles_after: Option<u32>,
    /// Every `reconnect`, failed ones included.
    pub reconnects: u32,
    /// Every `reconnect` fails with this error.
    pub reconnect_error: Option<ApiError>,
    /// Refuse this many Health calls with `RESOURCE_EXHAUSTED`, as kilnd's
    /// Health lane does when it is full, before answering again.
    pub health_refusals: u32,
    /// Answers every `/bin/grep`, instead of the fake's literal-only
    /// emulation (which refuses `-E`).
    pub grep_handler: Option<GrepHandler>,
    /// `find` lists files in reverse path order, as a filesystem may.
    pub find_reversed: bool,
    /// The stdout capture of every execution whose `argv[0]` is the first
    /// is changed by the second, as kilnd's would be when it kept only part.
    pub capture_tamper: Option<(&'static str, CaptureTamper)>,
    /// Symbolic links: each path's link text, a name relative to the
    /// link's directory. `head` follows one, as does `stat -L`; plain `stat`
    /// reports the link's own size, the length of its text.
    pub symlinks: BTreeMap<String, String>,
}

impl FakeState {
    /// The file `path` names, following one symbolic link.
    fn resolve(&self, path: &str) -> String {
        match self.symlinks.get(path) {
            Some(text) => match path.rsplit_once('/') {
                Some((dir, _)) => format!("{dir}/{text}"),
                None => text.clone(),
            },
            None => path.to_string(),
        }
    }
}

impl FakeApi {
    pub fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().expect("fake state lock")
    }

    /// A fake of kilnd 853dee2's expiry and typed reasons, with its default
    /// bounds, beside the exec options.
    pub fn with_expiry() -> Self {
        let api = Self::default();
        {
            let mut state = api.state();
            state
                .features
                .extend([Feature::SandboxExpiry, Feature::TypedReasons]);
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
        if baseline {
            let error = state.baseline_exec_error.clone()?;
            state.execs.push(request.clone());
            return Some((error, Duration::ZERO));
        }
        let error = state
            .exec_refusals
            .pop_front()
            .or_else(|| state.tool_exec_error.clone())?;
        state.execs.push(request.clone());
        Some((error, state.tool_exec_delay))
    }

    /// Record an `Exec` or `ExecStream` request as kilnd would: refused before
    /// admission, or a row with its output, how long its answer takes, and
    /// whether that answer is lost.
    #[allow(clippy::too_many_lines)]
    async fn record_exec(
        &self,
        request: SandboxExec,
        mode: ExecutionMode,
    ) -> Result<(Execution, Duration, bool), ApiError> {
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
        let recorded = {
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
                        fake_find(&state, argv)
                    }
                }
                Some("/bin/grep") => match &state.grep_handler {
                    Some(handler) => {
                        let answer = handler(argv, &state.files);
                        (answer.exit_code.unwrap_or(0), answer.stdout, answer.stderr)
                    }
                    None => fake_grep(&state.files, argv),
                },
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
                    let size = match state.symlinks.get(path) {
                        Some(text) if !argv.iter().any(|arg| arg == "-L") => Some(text.len()),
                        _ => state.files.get(&state.resolve(path)).map(Vec::len),
                    };
                    match size {
                        Some(size) => (0, format!("{size}\n").into_bytes(), Vec::new()),
                        None => (1, Vec::new(), b"not found".to_vec()),
                    }
                }
                Some("/usr/bin/head") => {
                    let limit = argv[2].parse::<usize>().expect("head limit");
                    let path = state.resolve(argv.last().expect("head path"));
                    let stored = state.read_override.as_ref().or(state.files.get(&path));
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
            let (cwd, env_names, env_sha256) = recorded_context(&request);
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
                cwd,
                env_names,
                env_sha256,
                mode,
            };
            if let Some((exec_state, exec_code)) = sh_state {
                execution.state = exec_state;
                execution.exit_code = exec_code;
            }
            if let Some((program, tamper)) = state.capture_tamper
                && argv.first().map(String::as_str) == Some(program)
            {
                tamper(&mut execution.stdout);
            }
            if let Some((number, tamper)) = state.tamper_exec
                && number == state.execs.len()
            {
                tamper(&mut execution);
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
        Ok(recorded)
    }

    /// kilnd under `--output-storage full` keeps a prefix of whatever a
    /// command wrote, so a scripted stream's output shows in its row: a
    /// stream with live bytes where the row has none gets them (the chunks'
    /// bytes, or a placeholder for gaps alone) and observes at least as many
    /// as the stream shows. A scripted tamper of this Exec then applies
    /// again, so it still has the last word.
    fn retain_live_output(&self, execution: &mut Execution, events: &[StreamEvent]) {
        let mut state = self.state();
        for stream in [LiveStream::Stdout, LiveStream::Stderr] {
            let mut data = Vec::new();
            let mut end = 0u64;
            for event in events {
                match event {
                    StreamEvent::Chunk(at, offset, bytes) if *at == stream => {
                        if data.len() < 1 << 20 {
                            data.extend_from_slice(bytes);
                        }
                        end = end.max(offset.saturating_add(bytes.len() as u64));
                    }
                    StreamEvent::Gap(at, offset, dropped, _) if *at == stream => {
                        end = end.max(offset.saturating_add(*dropped));
                    }
                    _ => {}
                }
            }
            let output = match stream {
                LiveStream::Stdout => &mut execution.stdout,
                LiveStream::Stderr => &mut execution.stderr,
            };
            if end == 0 || !output.data.is_empty() {
                continue;
            }
            if data.is_empty() {
                data = b"x".to_vec();
            }
            data.truncate(1 << 20);
            output.observed_bytes = output.observed_bytes.max(end);
            output.truncated = (data.len() as u64) < output.observed_bytes;
            output.data = data;
        }
        if let Some((number, tamper)) = state.tamper_exec
            && number == state.execs.len()
        {
            tamper(execution);
        }
        if let Some(row) = state
            .executions
            .iter_mut()
            .find(|row| row.id == execution.id)
        {
            *row = execution.clone();
        }
    }

    /// A dropped stream cancels its execution: the row becomes CANCELED for
    /// a client disconnect, still reusable, at once or after
    /// `stopped_settles_after` reads.
    fn drop_stream(&self, execution: &Execution) {
        let mut state = self.state();
        state.streams_dropped += 1;
        let stopped = Execution {
            state: ExecutionState::Canceled,
            exit_code: None,
            failure_reason: "client disconnected".to_string(),
            cancel_reason: Some(CancelReason::ClientDisconnected),
            ..execution.clone()
        };
        let settles_after = state.stopped_settles_after;
        let Some(row) = state
            .executions
            .iter_mut()
            .find(|row| row.id == execution.id)
        else {
            return;
        };
        match settles_after {
            None => *row = stopped,
            Some(reads) => {
                *row = Execution {
                    state: ExecutionState::Unknown,
                    ..stopped.clone()
                };
                state.pending_terminal = Some((stopped, reads));
            }
        }
    }

    /// Expire the sandbox now, as kilnd's sweeper would.
    pub fn expire(&self, reason: &'static str) {
        self.state().expired = Some(reason);
    }
}

/// kilnd 853dee2's default expiry bounds, with kilnd 0c79944's default
/// exec-option bounds.
pub fn kilnd_limits() -> ApiLimits {
    ApiLimits {
        min_sandbox_lifetime_ms: 10_000,
        max_sandbox_lifetime_ms: 7 * 86_400_000,
        default_sandbox_lifetime_ms: 0,
        min_sandbox_idle_ms: 10_000,
        max_sandbox_idle_ms: 86_400_000,
        default_sandbox_idle_ms: 0,
        ..exec_limits()
    }
}

/// kilnd 0c79944's default exec-option bounds, which it advertises while
/// it lists the exec options, and kilnd 2562a53's default stream timeouts
/// (30 minutes by default, 2 hours at most); no expiry bounds.
pub fn exec_limits() -> ApiLimits {
    ApiLimits {
        max_cwd_bytes: 4096,
        max_env_entries: 64,
        max_env_name_bytes: 128,
        max_env_value_bytes: 4096,
        max_env_total_bytes: 32_768,
        default_stream_timeout_ms: 1_800_000,
        max_stream_timeout_ms: 7_200_000,
        ..ApiLimits::default()
    }
}

/// What kilnd records for an execution of `request`: its cwd as sent, the
/// env names sorted, and their digest.
pub fn recorded_context(request: &SandboxExec) -> (String, Vec<String>, String) {
    (
        request.cwd.clone(),
        env_names(&request.env),
        env_sha256(&request.env),
    )
}

/// The env overrides every live execution sends, in order.
pub fn workspace_env() -> Vec<EnvVar> {
    [("TERM", "dumb"), ("NO_COLOR", "1"), ("PAGER", "cat")]
        .map(|(name, value)| EnvVar {
            name: name.to_string(),
            value: value.to_string(),
        })
        .to_vec()
}

/// `execution` as kilnd records it for an Exec of a live session, which
/// sends `/workspace` and the fixed overrides.
pub fn in_workspace(execution: Execution) -> Execution {
    let env = workspace_env();
    Execution {
        cwd: "/workspace".to_string(),
        env_names: env_names(&env),
        env_sha256: env_sha256(&env),
        ..execution
    }
}

/// The same record for a wire request, as a stub kilnd returns it.
pub fn pb_recorded_context(
    request: &kiln_harness::pb::ExecRequest,
) -> (String, Vec<String>, String) {
    let env: Vec<EnvVar> = request
        .env
        .iter()
        .map(|var| EnvVar {
            name: var.name.clone(),
            value: var.value.clone(),
        })
        .collect();
    (request.cwd.clone(), env_names(&env), env_sha256(&env))
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
            if state.health_refusals > 0 {
                state.health_refusals -= 1;
                return Err(api_error(
                    Code::ResourceExhausted,
                    "Health RPC capacity reached",
                ));
            }
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

    async fn exec(&self, request: SandboxExec) -> Result<Execution, ApiError> {
        let (execution, delay, lost) = self.record_exec(request, ExecutionMode::Unary).await?;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if lost {
            return Err(api_error(Code::Unavailable, "exec response lost"));
        }
        Ok(execution)
    }

    async fn exec_stream(
        &self,
        request: SandboxExec,
        observer: &mut (dyn StreamObserver + Send),
    ) -> Result<StreamedExec, ApiError> {
        self.state().streams.push(request.clone());
        let (mut execution, delay, lost) = self.record_exec(request, ExecutionMode::Stream).await?;
        let script = self.state().stream_script.pop_front().unwrap_or_default();
        if let Some(events) = &script.events {
            self.retain_live_output(&mut execution, events);
        }
        let id = execution.id.clone();
        if let StreamEnd::BrokenBeforeStarted(error) = script.end {
            tokio::time::sleep(delay).await;
            return Ok(StreamedExec::Broken {
                execution_id: None,
                error,
                protocol_violation: false,
            });
        }
        observer.started(&id);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if lost {
            return Ok(StreamedExec::Broken {
                execution_id: Some(id),
                error: api_error(Code::Unavailable, "exec response lost"),
                protocol_violation: false,
            });
        }
        let events = script.events.unwrap_or_else(|| output_events(&execution));
        let mut delivery = [LiveDelivery {
            dropped_bytes_known: true,
            ..LiveDelivery::default()
        }; 2];
        for event in events {
            let flow = match event {
                StreamEvent::Chunk(stream, offset, data) => {
                    delivery[stream as usize].delivered_bytes += data.len() as u64;
                    observer.chunk(stream, offset, &data)
                }
                StreamEvent::Gap(stream, offset, dropped, known) => {
                    delivery[stream as usize].dropped_bytes += dropped;
                    delivery[stream as usize].dropped_bytes_known &= known;
                    observer.gap(stream, offset, dropped, known)
                }
                StreamEvent::Sleep(duration) => {
                    tokio::time::sleep(duration).await;
                    std::ops::ControlFlow::Continue(())
                }
            };
            if flow.is_break() {
                self.drop_stream(&execution);
                return Ok(StreamedExec::Stopped { execution_id: id });
            }
        }
        match script.end {
            StreamEnd::Broken(error) => Ok(StreamedExec::Broken {
                execution_id: Some(id),
                error,
                protocol_violation: false,
            }),
            StreamEnd::Violation => {
                // The adapter drops the stream at a violation: kilnd cancels.
                self.drop_stream(&execution);
                Ok(StreamedExec::Broken {
                    execution_id: Some(id),
                    error: api_error(
                        Code::Internal,
                        "ExecStream messages were not numbered in order",
                    ),
                    protocol_violation: true,
                })
            }
            StreamEnd::Finished | StreamEnd::BrokenBeforeStarted(_) => Ok(StreamedExec::Finished {
                execution,
                stdout: delivery[0],
                stderr: delivery[1],
            }),
        }
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
        state.get_execution_ids.push(execution_id.to_string());
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
        // As kilnd's `GetSandbox`: recent rows leave output data out.
        Ok(state
            .executions
            .iter()
            .rev()
            .map(|row| {
                let mut row = row.clone();
                row.stdout.data.clear();
                row.stderr.data.clear();
                row
            })
            .collect())
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

/// A new connection is a clone: it shares this fake's state.
#[async_trait]
impl Reconnect for FakeApi {
    async fn reconnect(&self) -> Result<Self, ApiError> {
        let mut state = self.state();
        state.reconnects += 1;
        match &state.reconnect_error {
            Some(error) => Err(error.clone()),
            None => Ok(self.clone()),
        }
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

/// `find ROOT -xdev -type f -print0` over the fake's files: every file
/// when ROOT is `/workspace`, otherwise ROOT itself or the files under it,
/// and exit 1 when there is none.
fn fake_find(state: &FakeState, argv: &[String]) -> (i32, Vec<u8>, Vec<u8>) {
    let root = argv.get(1).map_or("/workspace", String::as_str);
    let under = format!("{root}/");
    let mut paths = state
        .files
        .keys()
        .filter(|path| root == "/workspace" || *path == root || path.starts_with(&under))
        .collect::<Vec<_>>();
    if paths.is_empty() && root != "/workspace" {
        return (
            1,
            Vec::new(),
            format!("find: {root}: No such file or directory\n").into_bytes(),
        );
    }
    if state.find_reversed {
        paths.reverse();
    }
    let mut output = Vec::new();
    for path in paths {
        output.extend_from_slice(path.as_bytes());
        output.push(0);
    }
    (0, output, Vec::new())
}

/// `grep -n -H -F [-i] -e PATTERN -- FILE...` over the fake's files, in
/// argv order, as busybox prints it: exit 0 with a match, 1 without, 2
/// when a file is missing (with a line on stderr). Anything else,
/// `-E` included, exits 2: the fake matches literal strings only.
fn fake_grep(files: &BTreeMap<String, Vec<u8>>, argv: &[String]) -> (i32, Vec<u8>, Vec<u8>) {
    let unsupported = |what: &str| {
        (
            2,
            Vec::new(),
            format!("fake: {what} unsupported\n").into_bytes(),
        )
    };
    let mut words = argv[1..].iter().map(String::as_str);
    let mut ignore_case = false;
    let mut pattern = None;
    loop {
        match words.next() {
            Some("-n" | "-H" | "-F") => {}
            Some("-i") => ignore_case = true,
            Some("-E") => return unsupported("-E"),
            Some("-e") => pattern = words.next(),
            Some("--") => break,
            Some(other) => return unsupported(other),
            None => return unsupported("an argv without --"),
        }
    }
    let Some(pattern) = pattern else {
        return unsupported("an argv without -e");
    };
    let fold = |bytes: &[u8]| -> Vec<u8> {
        if ignore_case {
            bytes.to_ascii_lowercase()
        } else {
            bytes.to_vec()
        }
    };
    let needle = fold(pattern.as_bytes());
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let (mut matched, mut failed) = (false, false);
    for file in words {
        let Some(bytes) = files.get(file) else {
            failed = true;
            stderr
                .extend_from_slice(format!("grep: {file}: No such file or directory\n").as_bytes());
            continue;
        };
        let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        if bytes.is_empty() {
            continue;
        }
        for (index, line) in body.split(|byte| *byte == b'\n').enumerate() {
            if fold(line)
                .windows(needle.len())
                .any(|window| window == needle.as_slice())
            {
                matched = true;
                stdout.extend_from_slice(format!("{file}:{}:", index + 1).as_bytes());
                stdout.extend_from_slice(line);
                stdout.push(b'\n');
            }
        }
    }
    let code = if failed { 2 } else { i32::from(!matched) };
    (code, stdout, stderr)
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

/// The default stream of a row: its retained stdout, then stderr, each as
/// one chunk at offset 0.
pub fn output_events(execution: &Execution) -> Vec<StreamEvent> {
    [
        (LiveStream::Stdout, &execution.stdout.data),
        (LiveStream::Stderr, &execution.stderr.data),
    ]
    .into_iter()
    .filter(|(_, data)| !data.is_empty())
    .map(|(stream, data)| StreamEvent::Chunk(stream, 0, data.clone()))
    .collect()
}

/// An `ExecStream` answer made from a unary answer, for wrappers whose
/// `exec` decides the outcome: a refusal stays one, and an execution is
/// streamed as its output and finished, recorded as streamed.
pub fn stream_from_exec(
    result: Result<Execution, ApiError>,
    observer: &mut (dyn StreamObserver + Send),
) -> Result<StreamedExec, ApiError> {
    let execution = Execution {
        mode: ExecutionMode::Stream,
        ..result?
    };
    observer.started(&execution.id);
    for event in output_events(&execution) {
        if let StreamEvent::Chunk(stream, offset, data) = event
            && observer.chunk(stream, offset, &data).is_break()
        {
            return Ok(StreamedExec::Stopped {
                execution_id: execution.id,
            });
        }
    }
    let delivered = |data: &[u8]| LiveDelivery {
        delivered_bytes: data.len() as u64,
        dropped_bytes: 0,
        dropped_bytes_known: true,
    };
    Ok(StreamedExec::Finished {
        stdout: delivered(&execution.stdout.data),
        stderr: delivered(&execution.stderr.data),
        execution,
    })
}

/// A provider that also hashes each request as a session in an earlier
/// execution context would have sent it: the same request with the tools as
/// a session that does not stream describes them, whose `exec` description
/// does not state the output cutoff.
pub struct Unstreamed<P> {
    pub inner: P,
    /// Per request: its hash, and its hash with the earlier tools.
    pub hashes: Vec<(String, String)>,
    tools: Vec<kiln_harness::provider::ToolDefinition>,
}

impl<P> Unstreamed<P> {
    pub fn new(inner: P) -> Self {
        Self {
            inner,
            hashes: Vec::new(),
            tools: kiln_harness::tools::ToolRegistry::new(HarnessConfig::default().tool_limits)
                .definitions()
                .to_vec(),
        }
    }
}

#[async_trait]
impl<P: kiln_harness::provider::Provider> kiln_harness::provider::Provider for Unstreamed<P> {
    fn info(&self) -> kiln_harness::provider::ProviderInfo {
        self.inner.info()
    }

    async fn complete(
        &mut self,
        request: kiln_harness::provider::ModelRequest<'_>,
    ) -> Result<kiln_harness::provider::ModelResponse, kiln_harness::provider::ProviderError> {
        let earlier = kiln_harness::provider::ModelRequest {
            tools: &self.tools,
            ..request
        };
        self.hashes.push((
            kiln_harness::journal::request_sha256(&request),
            kiln_harness::journal::request_sha256(&earlier),
        ));
        self.inner.complete(request).await
    }

    fn tool_completed(
        &mut self,
        turn: u32,
        index: u32,
        call: &kiln_harness::provider::ToolCall,
        ok: bool,
        result: &serde_json::Value,
    ) -> Result<(), kiln_harness::provider::ProviderError> {
        self.inner.tool_completed(turn, index, call, ok, result)
    }

    fn check_completed(&mut self, outcome: &kiln_harness::engine::CheckOutcome) {
        self.inner.check_completed(outcome);
    }
}

/// A live recording made through [`Unstreamed`] with `hashes`, as the same
/// session recorded in the earlier execution context `context` (`None` for
/// legacy): that context, the earlier tool definitions, and each request's
/// hash with them. Every tool result stays as recorded.
pub fn recorded_in(
    records: &[kiln_harness::journal::JournalRecord],
    context: Option<&str>,
    hashes: &[(String, String)],
) -> Vec<kiln_harness::journal::JournalRecord> {
    use kiln_harness::journal::JournalEvent;
    let tools = kiln_harness::tools::ToolRegistry::new(HarnessConfig::default().tool_limits)
        .definitions()
        .to_vec();
    let mut hashes = hashes.iter();
    let mut records = records.to_vec();
    for record in &mut records {
        match &mut record.event {
            JournalEvent::SessionStarted {
                exec_context,
                tools: recorded,
                ..
            } => {
                assert_eq!(exec_context.as_deref(), Some("workspace_stream_v1"));
                *exec_context = context.map(str::to_string);
                recorded.clone_from(&tools);
            }
            JournalEvent::ModelRequested { request_sha256, .. } => {
                let (live, earlier) = hashes.next().expect("a hash per request");
                assert_eq!(request_sha256, live);
                request_sha256.clone_from(earlier);
            }
            _ => {}
        }
    }
    assert!(hashes.next().is_none(), "a request per hash");
    records
}
