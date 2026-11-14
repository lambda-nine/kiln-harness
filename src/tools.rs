use std::collections::BTreeSet;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::time::{Instant, sleep_until, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::api::{
    ApiError, ApiLimits, CancelReason, CapturedOutput, EnvVar, Execution, ExecutionMode,
    ExecutionState, ExpiryReason, LiveDelivery, LiveStream, SandboxApi, SandboxExec,
    StreamObserver, StreamedExec, exec_stream_limit, execution_start_raced,
    sandbox_unusable_refusal,
};
use crate::audit::{ContentSummary, LiveEvidence, OutputEvidence};
use crate::engine::{CheckOutcome, SeedFile, TaskCheck};
use crate::journal::SessionLimits;
use crate::live::LiveOutput;
use crate::provider::{ToolCall, ToolDefinition};

mod coding;

use coding::EditArgs;

const WORKSPACE: &str = "/workspace";

/// Which tools a session gives the model. The journal records it in
/// `session_started`, so replay offers a recording the tools it had.
/// Every constant of the coding tools belongs to [`ToolSet::CodingV1`]:
/// changing any of them needs a new set name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolSet {
    /// `exec`, `read_file`, `write_file`, `list_dir` and `workspace_diff`.
    #[default]
    Base,
    /// The base tools, with line ranges in `read_file`, plus `edit_file`
    /// and `search`.
    CodingV1,
}

impl ToolSet {
    /// The `session_started.tool_set` value; `None` for [`ToolSet::Base`],
    /// which journals omit, so base journals are unchanged.
    #[must_use]
    pub fn journal_name(self) -> Option<&'static str> {
        match self {
            Self::Base => None,
            Self::CodingV1 => Some("coding_v1"),
        }
    }

    /// The set a recorded `tool_set` names: absent is base, and any value
    /// but `coding_v1` is unknown (`None`).
    #[must_use]
    pub fn from_journal(name: Option<&str>) -> Option<Self> {
        match name {
            None => Some(Self::Base),
            Some("coding_v1") => Some(Self::CodingV1),
            Some(_) => None,
        }
    }

    /// The name eval manifests and reports use: `base` or `coding_v1`.
    #[must_use]
    pub fn report_name(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::CodingV1 => "coding_v1",
        }
    }
}

/// How much of a file a ranged `read_file` scans for lines.
pub const MAX_SCAN_BYTES: usize = 1 << 20;
/// The most lines one ranged `read_file` returns, and its default.
pub const MAX_LINE_COUNT: u32 = 5000;
/// The highest `start_line` a ranged `read_file` accepts.
pub const MAX_START_LINE: u32 = 10_000_000;
/// The most edits one `edit_file` call carries.
pub const MAX_EDITS: usize = 32;
/// The most replacements one `edit_file` call makes, over all its edits.
pub const MAX_REPLACEMENTS: usize = 10_000;
/// The longest `search` pattern.
pub const MAX_PATTERN_BYTES: usize = 1024;
/// The most matches one `search` returns.
pub const MAX_SEARCH_RESULTS: u32 = 200;
/// The matches `search` returns without `max_results`.
pub const DEFAULT_SEARCH_RESULTS: u32 = 50;
/// The longest match text `search` returns before cutting it.
pub const SEARCH_TEXT_BYTES: usize = 300;
/// How much of a failed command's stderr a coding tool's error carries.
pub const ERROR_STDERR_BYTES: usize = 1024;
/// Room left for a result's other fields when its content is budgeted
/// against the result limit.
pub const RESULT_RESERVE_BYTES: usize = 1024;

/// The environment every execution gets over kiln's defaults, in order.
/// The output goes to a model, so no colour or other escape codes
/// (`TERM=dumb`, `NO_COLOR=1`) and no pager waiting on a terminal
/// (`PAGER=cat`). `HOME` stays `/root` on purpose: the root filesystem is
/// read-only, and a writable `HOME` would put dotfiles in `/workspace` and
/// its diff. These are constants, never from the model, a task, the CLI or
/// the host environment, because kilnd stores and hashes env values.
const EXEC_ENV: [(&str, &str); 3] = [("TERM", "dumb"), ("NO_COLOR", "1"), ("PAGER", "cat")];

/// Where every execution of a session runs and which env overrides it
/// gets. The journal records it in `session_started`, so replay runs a
/// recording's executions as they ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecContext {
    /// Before kilnd's exec options: no cwd (kilnd's `/`) and no overrides.
    /// Only replay of a journal recorded without `exec_context` uses it.
    Legacy,
    /// `/workspace` with [`EXEC_ENV`], every execution unary. Only replay
    /// of a journal recorded before streaming uses it.
    WorkspaceV1,
    /// [`ExecContext::WorkspaceV1`], except that the model's `exec` calls go
    /// through `ExecStream`: their output reaches the host live, and one
    /// whose output passes [`RUNAWAY_OUTPUT_BYTES`] is stopped. Every live
    /// session uses it.
    WorkspaceStreamV1,
}

impl ExecContext {
    /// The `session_started.exec_context` value; `None` for
    /// [`ExecContext::Legacy`], which journals omit.
    #[must_use]
    pub(crate) fn journal_name(self) -> Option<&'static str> {
        match self {
            Self::Legacy => None,
            Self::WorkspaceV1 => Some("workspace_v1"),
            Self::WorkspaceStreamV1 => Some("workspace_stream_v1"),
        }
    }

    /// The context a recorded `exec_context` names: absent is legacy, and
    /// any value but `workspace_v1` or `workspace_stream_v1` is unknown
    /// (`None`).
    #[must_use]
    pub(crate) fn from_journal(name: Option<&str>) -> Option<Self> {
        match name {
            None => Some(Self::Legacy),
            Some("workspace_v1") => Some(Self::WorkspaceV1),
            Some("workspace_stream_v1") => Some(Self::WorkspaceStreamV1),
            Some(_) => None,
        }
    }

    /// Whether executions send cwd and env, which needs `FEATURE_EXEC_CWD`,
    /// `FEATURE_EXEC_ENV` and room for them in the advertised limits.
    #[must_use]
    pub(crate) fn sends_exec_options(self) -> bool {
        matches!(self, Self::WorkspaceV1 | Self::WorkspaceStreamV1)
    }

    /// Whether the model's `exec` calls go through `ExecStream`, which
    /// needs `FEATURE_EXEC_STREAM` and a stream timeout that fits the tool
    /// timeout. Every other execution stays unary.
    #[must_use]
    pub(crate) fn streams_exec(self) -> bool {
        self == Self::WorkspaceStreamV1
    }

    /// The `ExecRequest.cwd` every execution sends; empty is kilnd's `/`.
    #[must_use]
    pub(crate) fn cwd(self) -> &'static str {
        match self {
            Self::Legacy => "",
            Self::WorkspaceV1 | Self::WorkspaceStreamV1 => WORKSPACE,
        }
    }

    /// The `ExecRequest.env` every execution sends, in order.
    #[must_use]
    pub(crate) fn env(self) -> Vec<EnvVar> {
        match self {
            Self::Legacy => Vec::new(),
            Self::WorkspaceV1 | Self::WorkspaceStreamV1 => EXEC_ENV
                .iter()
                .map(|(name, value)| EnvVar {
                    name: (*name).to_string(),
                    value: (*value).to_string(),
                })
                .collect(),
        }
    }

    /// Whether kilnd recorded exactly this context for `execution`.
    #[must_use]
    pub(crate) fn ran_as_sent(self, execution: &Execution) -> bool {
        execution.ran_with(self.cwd(), &self.env())
    }
}

/// The problem, if any, with running [`ExecContext::WorkspaceV1`] under
/// `limits`: the first advertised exec limit too small for `/workspace` or
/// the fixed overrides. Zero is too small, since a daemon fills these
/// limits whenever it lists the features.
#[must_use]
pub(crate) fn exec_limits_problem(limits: &ApiLimits) -> Option<String> {
    let env = ExecContext::WorkspaceV1.env();
    let needed = |bytes: usize| u32::try_from(bytes).unwrap_or(u32::MAX);
    let checks = [
        (
            "max_cwd_bytes",
            limits.max_cwd_bytes,
            needed(WORKSPACE.len()),
        ),
        ("max_env_entries", limits.max_env_entries, needed(env.len())),
        (
            "max_env_name_bytes",
            limits.max_env_name_bytes,
            needed(env.iter().map(|var| var.name.len()).max().unwrap_or(0)),
        ),
        (
            "max_env_value_bytes",
            limits.max_env_value_bytes,
            needed(env.iter().map(|var| var.value.len()).max().unwrap_or(0)),
        ),
        (
            "max_env_total_bytes",
            limits.max_env_total_bytes,
            needed(
                env.iter()
                    .map(|var| var.name.len() + var.value.len() + 2)
                    .sum(),
            ),
        ),
    ];
    checks
        .into_iter()
        .find(|(_, advertised, needed)| advertised < needed)
        .map(|(name, advertised, needed)| {
            format!("kilnd advertised {name} {advertised}; kiln-harness needs at least {needed}")
        })
}

/// The problem, if any, with streaming the model's commands under
/// `limits` with tools of `tool_timeout`: kilnd's longest stream timeout
/// must fit the tool timeout. Zero, as from a daemon that reports no
/// limits, is too small.
#[must_use]
pub(crate) fn stream_limits_problem(limits: &ApiLimits, tool_timeout: Duration) -> Option<String> {
    let needed = u64::try_from(tool_timeout.as_millis()).unwrap_or(u64::MAX);
    let advertised = u64::from(limits.max_stream_timeout_ms);
    (advertised < needed).then(|| {
        format!(
            "kilnd advertised max_stream_timeout_ms {advertised}; kiln-harness needs at least \
             {needed} for exec_stream"
        )
    })
}

/// The message a session ends with when kilnd's record of an execution
/// does not show the cwd and env overrides the harness sent.
pub const EXEC_CONTEXT_MISMATCH: &str =
    "kilnd ran an execution with a different cwd or env than requested";

/// The message a session ends with when kilnd's record of a streamed
/// execution does not say it was streamed.
pub const EXEC_MODE_MISMATCH: &str = "kilnd recorded a streamed execution as another mode";

/// The message a session ends with when kilnd's record of a streamed
/// execution holds no output bytes though it observed some: the daemon is
/// not retaining output, so a result built from the row would be empty.
pub const OUTPUT_NOT_RETAINED: &str = "kilnd did not retain the execution's output; kiln-harness \
     needs kilnd to run with --output-storage full";

/// The message a session ends with when a streamed execution's stream
/// ended without its result, and kilnd's row of it could not be read as
/// terminal within the call's time. It names no execution: it is part of a
/// model-visible result, which replay compares.
pub const STREAM_RESULT_UNREADABLE: &str =
    "the command's result could not be read after its stream ended";

/// The message a session ends with when kilnd broke the `ExecStream`
/// protocol after starting a command. It names no execution, as it is part
/// of a model-visible result.
pub const STREAM_PROTOCOL_VIOLATION: &str = "kilnd broke the ExecStream protocol";

/// Why a row kilnd returned for a streamed `exec` cannot be accepted, if it
/// can't. `live_end` is, per stream (stdout, stderr), the furthest offset
/// the live stream proved: chunk `offset+len` and gap `offset+dropped`.
///
/// The row must show the session's cwd and env, say it was streamed, and
/// hold output wherever kilnd observed some or the live stream proved some.
/// Under `--output-storage none` kilnd stores no bytes and no byte count,
/// so only the live stream can prove the command wrote anything; a command
/// whose output was all lost live is not caught. An `INTERRUPTED` row is
/// exempt from the output check: kilnd's restart reconciliation marks a
/// running row so without its capture, whatever the storage mode, so its
/// empty output says nothing about retention.
fn streamed_row_problem(
    context: ExecContext,
    execution: &Execution,
    live_end: [u64; 2],
) -> Option<&'static str> {
    if !context.ran_as_sent(execution) {
        return Some(EXEC_CONTEXT_MISMATCH);
    }
    if execution.mode != ExecutionMode::Stream {
        return Some(EXEC_MODE_MISMATCH);
    }
    let withheld = |output: &CapturedOutput, live_end: u64| {
        output.data.is_empty() && (output.observed_bytes > 0 || live_end > 0)
    };
    if execution.state != ExecutionState::Interrupted
        && (withheld(&execution.stdout, live_end[0]) || withheld(&execution.stderr, live_end[1]))
    {
        return Some(OUTPUT_NOT_RETAINED);
    }
    None
}

/// A streamed command whose output on either stream is shown to pass this
/// many bytes, by delivered chunks and reported gaps, is stopped then
/// instead of running to its timeout. Live output is lossy: a command whose
/// excess output was dropped live and that then goes quiet is bounded by
/// its timeout instead.
pub const RUNAWAY_OUTPUT_BYTES: u64 = 8 << 20;

/// The `stopped` value of a result whose command passed
/// [`RUNAWAY_OUTPUT_BYTES`].
pub const OUTPUT_LIMIT: &str = "output_limit";

/// The name a call to an unregistered tool is recorded and shown under.
pub const UNKNOWN_TOOL: &str = "unknown";

/// The call the model sees and the journal records after host
/// sanitization: an unregistered name becomes `unknown`, and arguments over
/// the byte limit become an `omitted` marker. The engine and the journal
/// grammar both use this, so a recorded call can be checked against the
/// response it came from.
#[must_use]
pub fn sanitize_call(
    call: &ToolCall,
    tools: &[ToolDefinition],
    limits: &SessionLimits,
) -> ToolCall {
    sanitize_call_bounded(call, tools, limits.max_model_arguments_bytes).call
}

pub(crate) struct SanitizedCall {
    pub(crate) call: ToolCall,
    pub(crate) registered: bool,
    /// The original arguments' summary, or `None` when they exceeded the limit.
    pub(crate) arguments: Option<ContentSummary>,
}

pub(crate) fn sanitize_call_bounded(
    call: &ToolCall,
    tools: &[ToolDefinition],
    max_arguments_bytes: usize,
) -> SanitizedCall {
    let registered = tools.iter().find(|definition| definition.name == call.tool);
    // Encoding a JSON value cannot fail; treat a failure like an oversize.
    let arguments = ContentSummary::json_bounded(&call.arguments, max_arguments_bytes)
        .ok()
        .flatten();
    SanitizedCall {
        call: ToolCall {
            call_id: call.call_id.clone(),
            tool: registered.map_or_else(
                || UNKNOWN_TOOL.to_string(),
                |definition| definition.name.clone(),
            ),
            arguments: if arguments.is_some() {
                call.arguments.clone()
            } else {
                json!({"omitted": "arguments exceeded byte limit"})
            },
        },
        registered: registered.is_some(),
        arguments,
    }
}

#[derive(Debug, Clone)]
pub struct ToolLimits {
    pub timeout: Duration,
    pub max_argv_bytes: usize,
    pub max_stdin_bytes: usize,
    pub max_file_bytes: usize,
    pub max_result_bytes: usize,
    pub max_manifest_bytes: usize,
    pub max_manifest_entries: usize,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            max_argv_bytes: 32 << 10,
            max_stdin_bytes: 64 << 10,
            max_file_bytes: 64 << 10,
            max_result_bytes: 128 << 10,
            max_manifest_bytes: 64 << 10,
            max_manifest_entries: 512,
        }
    }
}

/// What the `exec` description adds in a session whose model commands
/// stream, so the cutoff is visible to the model whatever its system prompt.
pub const EXEC_STREAM_DESCRIPTION: &str =
    " A command whose stdout or stderr passes 8 MiB is stopped.";

#[derive(Debug, Clone)]
pub struct ToolRegistry {
    limits: ToolLimits,
    definitions: Vec<ToolDefinition>,
    /// The same tools as a streaming session describes them.
    streaming_definitions: Vec<ToolDefinition>,
    /// The [`ToolSet::CodingV1`] tools, unary and streaming.
    coding_definitions: Vec<ToolDefinition>,
    coding_streaming_definitions: Vec<ToolDefinition>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            definitions: definitions(false, ToolSet::Base, &limits),
            streaming_definitions: definitions(true, ToolSet::Base, &limits),
            coding_definitions: definitions(false, ToolSet::CodingV1, &limits),
            coding_streaming_definitions: definitions(true, ToolSet::CodingV1, &limits),
            limits,
        }
    }

    /// The tools of `tool_set`, described as a session that streams its
    /// model's commands (`streaming`) or one that does not.
    #[must_use]
    pub fn definitions_of(&self, tool_set: ToolSet, streaming: bool) -> &[ToolDefinition] {
        match (tool_set, streaming) {
            (ToolSet::Base, false) => &self.definitions,
            (ToolSet::Base, true) => &self.streaming_definitions,
            (ToolSet::CodingV1, false) => &self.coding_definitions,
            (ToolSet::CodingV1, true) => &self.coding_streaming_definitions,
        }
    }

    /// The tools as a session that does not stream its model's commands
    /// describes them: a recording made before streaming.
    #[must_use]
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    /// The tools as a session in `context` describes them. They are part of
    /// every model request, which replay compares by hash, so a recording
    /// keeps the definitions its context had: only a streaming session's
    /// `exec` states the output cutoff.
    /// The coding set's tools are described the same way, in either
    /// context.
    #[must_use]
    pub(crate) fn definitions_in(
        &self,
        context: ExecContext,
        tool_set: ToolSet,
    ) -> &[ToolDefinition] {
        self.definitions_of(tool_set, context.streams_exec())
    }

    /// Capture the fixed workspace baseline for a newly created sandbox.
    /// `ready_at` is a moment no later than kilnd made it ready, such as when
    /// create was sent: the first activity anchor, on the early side.
    /// `typed_reasons` says whether the daemon lists typed refusal reasons;
    /// only then does a tool call end on a refusal proving the sandbox gone.
    /// Every execution runs in `exec_context`, and the model's calls are
    /// planned as `tool_set` defines its tools.
    ///
    /// # Errors
    ///
    /// Returns an error if kilnd cannot execute or settle the bounded manifest.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_session<'a, A: SandboxApi>(
        &'a self,
        api: &'a A,
        sandbox_id: String,
        session_deadline: Instant,
        typed_reasons: bool,
        exec_context: ExecContext,
        ready_at: Instant,
        seed: &[SeedFile],
        tool_set: ToolSet,
    ) -> Result<(ToolSession<'a, A>, Option<SeedEvidence>, BaselineEvidence), ToolError> {
        let mut session = ToolSession {
            api,
            sandbox_id,
            exec_context,
            tool_set,
            limits: &self.limits,
            baseline: BTreeSet::new(),
            typed_reasons,
            // The sandbox has just become ready.
            activity: Mutex::new((ready_at, 0)),
            last_exec_sent: Mutex::new(None),
            unusable: Mutex::new(None),
            live: None,
            stream_call: Mutex::new(None),
            settled_call: Mutex::new(None),
        };
        // The seed goes in before the baseline, so the baseline includes it
        // and `workspace_diff` shows only the agent's changes.
        let seeded = if seed.is_empty() {
            None
        } else {
            Some(session.seed(seed, session_deadline).await?)
        };
        let manifest = session.manifest(session_deadline).await;
        if let Some(stop) = session.unusable() {
            let execution_ids = match &manifest {
                Ok(manifest) => manifest.execution_ids.clone(),
                Err(error) => error.execution_evidence().0,
            };
            return Err(ToolError::Unusable {
                stop,
                execution_ids,
            });
        }
        let manifest = manifest?;
        let evidence = BaselineEvidence {
            execution_ids: manifest.execution_ids,
            file_count: manifest.files.len(),
        };
        session.baseline = manifest.files;
        Ok((session, seeded, evidence))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineEvidence {
    pub execution_ids: Vec<String>,
    pub file_count: usize,
}

/// What seeding the workspace did: the files and bytes written and every
/// execution it ran, directory creations included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedEvidence {
    pub files: usize,
    pub bytes: u64,
    pub execution_ids: Vec<String>,
}

/// One run of a task's check: its verdict, the execution, and each output
/// stream for the journal, decoded as lossy UTF-8 and then cut to at most
/// [`CHECK_OUTPUT_BYTES`] bytes, with the audit's evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckRun {
    pub outcome: CheckOutcome,
    pub execution_id: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_evidence: OutputEvidence,
    pub stderr_evidence: OutputEvidence,
}

/// How much of each check output stream the journal keeps.
pub const CHECK_OUTPUT_BYTES: usize = 4 << 10;

pub struct ToolSession<'a, A: SandboxApi> {
    api: &'a A,
    sandbox_id: String,
    /// The cwd and env every execution sends, and kilnd must report.
    exec_context: ExecContext,
    /// Which tools the model has, and so how its calls are planned.
    tool_set: ToolSet,
    limits: &'a ToolLimits,
    baseline: BTreeSet<String>,
    typed_reasons: bool,
    /// The last activity the harness can prove kilnd saw (ready, a returned
    /// or settled recovered execution, or a renew the engine records), and
    /// a generation that every such record bumps, so activity in the same
    /// clock tick as something else is still ordered after it.
    activity: Mutex<(Instant, u64)>,
    /// When the latest Exec request was sent, the anchor for an execution
    /// found by recovery after its response was lost.
    last_exec_sent: Mutex<Option<Instant>>,
    /// Set once an execution reported that the sandbox cannot take another
    /// call, with kilnd's cancel reason if it gave one.
    unusable: Mutex<Option<ToolStop>>,
    /// Where streamed output goes on the host, if anywhere.
    live: Option<Arc<dyn LiveOutput>>,
    /// The streamed `exec` in flight or last run, for response-loss
    /// recovery. It lives outside the call's future, so a call dropped by
    /// cancellation or its deadline still leaves what it learned.
    stream_call: Mutex<Option<StreamCall>>,
    /// The executions the coding call in flight has settled, in order, with
    /// their evidence. It lives outside the call's future, so a call dropped
    /// by cancellation still leaves what it learned; see
    /// [`ToolSession::take_settled_call`].
    settled_call: Mutex<Option<EarlierExecutions>>,
}

/// What a streamed `exec` learned from its stream, kept for response-loss
/// recovery: the execution kilnd started, once `ExecStarted` arrived, and
/// per stream (stdout, stderr) the furthest offset the live stream proved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StreamCall {
    pub(crate) execution_id: Option<String>,
    pub(crate) live_end: [u64; 2],
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolRun {
    pub ok: bool,
    pub result: Value,
    pub execution_ids: Vec<String>,
    pub stdout: OutputEvidence,
    pub stderr: OutputEvidence,
    pub stop: Option<ToolStop>,
    /// How a streamed execution's output was delivered live; `None` for
    /// every unary call.
    pub live: Option<Box<LiveEvidence>>,
    /// Set when a coding tool's later execution has an unknown outcome
    /// (its RPC failed or its deadline passed) after earlier ones of the
    /// same call settled. `execution_ids` and the evidence then already hold
    /// those earlier executions, and response-loss recovery still looks for
    /// what follows them; see [`ToolRun::needs_recovery`].
    pub(crate) earlier: Option<EarlierExecutions>,
}

/// The executions a call settled before one whose outcome is unknown, with
/// their evidence alone.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EarlierExecutions {
    pub(crate) execution_ids: Vec<String>,
    pub(crate) stdout: OutputEvidence,
    pub(crate) stderr: OutputEvidence,
}

impl EarlierExecutions {
    fn empty() -> Self {
        Self {
            execution_ids: Vec::new(),
            stdout: empty_evidence(),
            stderr: empty_evidence(),
        }
    }

    fn of(executions: &[&Execution]) -> Self {
        let mut earlier = Self::empty();
        for execution in executions {
            earlier.push(execution);
        }
        earlier
    }

    /// Add `execution` after the others: its ID once, its evidence merged.
    fn push(&mut self, execution: &Execution) {
        if !self.execution_ids.contains(&execution.id) {
            self.execution_ids.push(execution.id.clone());
        }
        self.stdout = merge_evidence(self.stdout, evidence(&execution.stdout));
        self.stderr = merge_evidence(self.stderr, evidence(&execution.stderr));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredExecutionEvidence {
    pub execution_ids: Vec<String>,
    pub stdout: OutputEvidence,
    pub stderr: OutputEvidence,
    /// `Some(false)` when any recovered row left the sandbox unusable;
    /// otherwise what the rows said, or `None` from an older daemon.
    pub sandbox_reusable: Option<bool>,
    /// The cancel reason of the first recovered row that left the sandbox
    /// unusable.
    pub cancel_reason: Option<CancelReason>,
    /// A recovered execution still not terminal when recovery's deadline
    /// passed. Its outcome, and whether the sandbox survived it, is unknown.
    pub unsettled: Option<String>,
    /// No `GetSandbox` read succeeded in the whole window, so nothing is
    /// known about what kilnd assigned.
    pub unreadable: bool,
    /// The first recovered execution whose recorded cwd or env overrides
    /// differ from what the session sends, or, after a streamed call, whose
    /// row fails any check a returned streamed row must pass.
    pub mismatched: Option<String>,
    /// Why [`RecoveredExecutionEvidence::mismatched`] was refused; `None`
    /// reads as [`EXEC_CONTEXT_MISMATCH`].
    pub mismatch: Option<&'static str>,
}

impl RecoveredExecutionEvidence {
    /// How the session must end when a recovered row left the sandbox
    /// unusable, exactly as for a directly returned one.
    #[must_use]
    pub fn stop(&self) -> Option<ToolStop> {
        if self.mismatched.is_some() {
            return Some(ToolStop::Unusable(
                self.mismatch.unwrap_or(EXEC_CONTEXT_MISMATCH).to_string(),
            ));
        }
        if let Some(id) = &self.unsettled {
            return Some(ToolStop::Unusable(format!(
                "execution {id} did not settle after its response was lost"
            )));
        }
        if self.unreadable {
            return Some(ToolStop::Unusable(
                "execution evidence could not be read after its response was lost".to_string(),
            ));
        }
        (self.sandbox_reusable == Some(false)).then(|| unusable_stop(self.cancel_reason))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStop {
    Deadline,
    Canceled,
    /// kilnd refused the call because the sandbox expired or was deleted.
    /// The model-visible result is the same `kilnd_error` as before typed
    /// reasons; the session then ends. Also used when an execution left the
    /// sandbox unusable because it was deleted or reached its lifetime.
    Expired(ExpiryReason),
    /// An execution left the sandbox unusable for another or no stated
    /// cause, kilnd refused one because the sandbox is lost, quarantined,
    /// failed or not ready, or a recovered execution never settled. The
    /// session ends without asking the model again, with this message.
    Unusable(String),
}

/// The message of [`ToolStop::Unusable`] for a sandbox kilnd reported
/// unusable.
pub const UNUSABLE_SANDBOX: &str = "execution left the sandbox unusable";

impl ToolStop {
    fn unusable() -> Self {
        Self::Unusable(UNUSABLE_SANDBOX.to_string())
    }
}

impl ToolRun {
    fn rejected(kind: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: json!({"ok": false, "error": kind, "message": message.into()}),
            execution_ids: Vec::new(),
            stdout: empty_evidence(),
            stderr: empty_evidence(),
            stop: None,
            live: None,
            earlier: None,
        }
    }

    fn from_execution(execution: &Execution, result: Value, ok: bool) -> Self {
        Self {
            ok,
            result,
            execution_ids: vec![execution.id.clone()],
            stdout: evidence(&execution.stdout),
            stderr: evidence(&execution.stderr),
            stop: None,
            live: None,
            earlier: None,
        }
    }

    #[must_use]
    pub fn interrupted(stop: ToolStop) -> Self {
        Self {
            ok: false,
            result: json!({"ok": false, "error": "tool_interrupted"}),
            execution_ids: Vec::new(),
            stdout: unknown_evidence(),
            stderr: unknown_evidence(),
            stop: Some(stop),
            live: None,
            earlier: None,
        }
    }

    /// [`ToolRun::interrupted`] for a call that had already settled
    /// `earlier`, the executions a coding tool ran before it was dropped.
    /// They stay first in its IDs and evidence, the totals stay unknown, and
    /// response-loss recovery still looks for what may have followed them.
    /// With `None` it is exactly [`ToolRun::interrupted`].
    pub(crate) fn interrupted_after(stop: ToolStop, earlier: Option<EarlierExecutions>) -> Self {
        let mut run = Self::interrupted(stop);
        if let Some(earlier) = earlier {
            run.execution_ids.clone_from(&earlier.execution_ids);
            run.stdout = merge_evidence(earlier.stdout, run.stdout);
            run.stderr = merge_evidence(earlier.stderr, run.stderr);
            run.earlier = Some(earlier);
        }
        run
    }

    pub(crate) fn bounded(mut self, max_bytes: usize) -> Self {
        let summary = ContentSummary::json_bounded(&self.result, max_bytes);
        if !matches!(summary, Ok(Some(_))) {
            self.ok = false;
            self.result = json!({"ok": false, "error": "tool_result_too_large"});
            if self.stdout.observed_bytes > 0 {
                self.stdout.truncated = true;
            }
            if self.stderr.observed_bytes > 0 {
                self.stderr.truncated = true;
            }
        }
        self
    }

    /// Whether response-loss recovery should look for executions this call
    /// may have made that it does not know of: none is known and the
    /// evidence is unknown, or a coding tool's later execution has an
    /// unknown outcome after earlier ones it keeps.
    #[must_use]
    pub fn needs_recovery(&self) -> bool {
        (self.execution_ids.is_empty() || self.earlier.is_some())
            && (!self.stdout.total_bytes_known || !self.stderr.total_bytes_known)
    }

    /// Attach evidence recovered after a lost response. A recovered row that
    /// left the sandbox unusable stops the session like a returned one; an
    /// interruption already recorded keeps its own stop. Executions the
    /// call settled before the lost one stay first, and recovery, which
    /// looked past them, adds what it found after them; if it found
    /// nothing, they stay with the unknown evidence.
    pub fn attach_recovered(&mut self, recovered: RecoveredExecutionEvidence) {
        // A stop needs no IDs: an unreadable window has none.
        if self.stop.is_none() {
            self.stop = recovered.stop();
        }
        let earlier = self.earlier.take();
        if recovered.execution_ids.is_empty() {
            return;
        }
        if let Some(earlier) = earlier {
            let mut ids = earlier.execution_ids;
            for id in recovered.execution_ids {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            self.execution_ids = ids;
            self.stdout = merge_evidence(earlier.stdout, recovered.stdout);
            self.stderr = merge_evidence(earlier.stderr, recovered.stderr);
        } else {
            self.execution_ids = recovered.execution_ids;
            self.stdout = recovered.stdout;
            self.stderr = recovered.stderr;
        }
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error("session deadline expired")]
    Deadline,
    #[error("the baseline left the sandbox unusable: {stop:?}")]
    Unusable {
        /// How the session ends: [`ToolStop::Expired`] when kilnd's cancel
        /// reason proves an expiry, otherwise [`ToolStop::Unusable`].
        stop: ToolStop,
        execution_ids: Vec<String>,
    },
    #[error("{message}")]
    Seed {
        message: String,
        execution_ids: Vec<String>,
    },
    #[error("workspace manifest was invalid: {message}")]
    Manifest {
        message: String,
        execution_ids: Vec<String>,
        stdout: OutputEvidence,
        stderr: OutputEvidence,
    },
}

impl ToolError {
    #[must_use]
    pub fn execution_evidence(&self) -> (Vec<String>, OutputEvidence, OutputEvidence) {
        match self {
            Self::Manifest {
                execution_ids,
                stdout,
                stderr,
                ..
            } => (execution_ids.clone(), *stdout, *stderr),
            Self::Unusable { execution_ids, .. } | Self::Seed { execution_ids, .. } => (
                execution_ids.clone(),
                unknown_evidence(),
                unknown_evidence(),
            ),
            Self::Api(_) | Self::Deadline => (Vec::new(), unknown_evidence(), unknown_evidence()),
        }
    }
}

impl<A: SandboxApi> ToolSession<'_, A> {
    /// When kilnd last saw activity on the sandbox, as far as this session
    /// knows.
    #[must_use]
    pub fn last_activity(&self) -> Instant {
        self.activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
    }

    /// How many activities have been recorded. It only grows, so a larger
    /// value proves activity after an earlier reading, whatever the clock.
    #[must_use]
    pub fn activity_generation(&self) -> u64 {
        self.activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1
    }

    /// Record activity proven by a request sent at `sent`, such as a
    /// successful renew. kilnd committed the activity somewhere between the
    /// send and the reply, so the send time is the anchor: early, never
    /// late. The anchor never moves back; the generation always advances.
    pub fn record_activity(&self, sent: Instant) {
        let mut activity = self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        activity.0 = activity.0.max(sent);
        activity.1 = activity.1.saturating_add(1);
    }

    /// The cwd and env every execution of this session sends.
    pub(crate) fn exec_context(&self) -> ExecContext {
        self.exec_context
    }

    /// Send streamed output to `live` from now on. Host-only: it never
    /// changes a result or a record.
    pub(crate) fn set_live_output(&mut self, live: Option<Arc<dyn LiveOutput>>) {
        self.live = live;
    }

    fn unusable(&self) -> Option<ToolStop> {
        self.unusable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub async fn recover_new_execution_evidence(
        &self,
        known: &BTreeSet<String>,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> RecoveredExecutionEvidence {
        // What the streamed call this recovery follows learned, if it was
        // one. An execution already known belongs to an earlier call that
        // finished: a call dropped before it ran learned nothing.
        let streamed = self
            .stream_call
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .filter(|call| {
                call.execution_id
                    .as_ref()
                    .is_none_or(|id| !known.contains(id))
            });
        let recovered = recover_execution_evidence_in(
            self.api,
            &self.sandbox_id,
            self.exec_context,
            known,
            deadline,
            cancel,
            streamed,
        )
        .await;
        // A settled recovered row is an execution kilnd admitted and
        // finished, so it is activity; one that left the sandbox unusable,
        // or never settled, refuses any later Exec.
        if !recovered.execution_ids.is_empty()
            && recovered.unsettled.is_none()
            && let Some(sent) = *self
                .last_exec_sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            self.record_activity(sent);
        }
        if let Some(stop) = recovered.stop() {
            self.unusable
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_or_insert(stop);
        }
        recovered
    }

    /// Record that the coding call in flight settled `execution`, after the
    /// ones it settled before.
    pub(crate) fn record_settled(&self, execution: &Execution) {
        self.settled_call
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(EarlierExecutions::empty)
            .push(execution);
    }

    /// What a coding call dropped before it returned had settled, in order,
    /// for [`ToolRun::interrupted_after`]; `None` when it settled nothing,
    /// or when the call was not a coding tool's. Each call starts and ends
    /// with this cleared, so it never names an earlier call's executions.
    pub(crate) fn take_settled_call(&self) -> Option<EarlierExecutions> {
        self.settled_call
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Whether `tool` with `arguments` would reach kilnd: a registered tool
    /// whose arguments pass every host-side check. Unknown tools and invalid
    /// arguments are answered on the host and send nothing.
    #[must_use]
    pub fn contacts_kilnd(&self, tool: &str, arguments: &Value) -> bool {
        self.plan(tool, arguments.clone()).is_ok()
    }

    /// Run one tool call. The tool timeout bounds the Execs it sends
    /// together: each gets the time that remains of it. Response-loss
    /// recovery, if the engine runs it, comes after.
    pub async fn execute(
        &self,
        tool: &str,
        arguments: Value,
        session_deadline: Instant,
    ) -> ToolRun {
        let deadline = session_deadline.min(Instant::now() + self.limits.timeout);
        self.execute_call(tool, arguments, session_deadline, "", deadline)
            .await
    }

    /// [`ToolSession::execute`] for the call the host audits as `call_id`,
    /// the name its streamed output goes to the live sink under. A streamed
    /// `exec` whose stream ended without its result reads kilnd's row of it
    /// until `follow_deadline`, which may reach past the call's own time
    /// into the window response-loss recovery would otherwise use.
    pub(crate) async fn execute_call(
        &self,
        tool: &str,
        arguments: Value,
        session_deadline: Instant,
        call_id: &str,
        follow_deadline: Instant,
    ) -> ToolRun {
        *self
            .stream_call
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.take_settled_call();
        let deadline = session_deadline.min(Instant::now() + self.limits.timeout);
        let run = match self.plan(tool, arguments) {
            Err(rejected) => rejected,
            Ok(ToolPlan::Exec(plan)) => {
                self.exec_tool(plan, deadline, call_id, follow_deadline)
                    .await
            }
            Ok(ToolPlan::Read { path, max_bytes }) => {
                self.read_file(path, max_bytes, deadline).await
            }
            Ok(ToolPlan::ReadLines {
                path,
                start,
                count,
                max_bytes,
            }) => {
                self.read_lines(path, start, count, max_bytes, deadline)
                    .await
            }
            Ok(ToolPlan::Write { path, bytes }) => self.write_file(path, bytes, deadline).await,
            Ok(ToolPlan::Edit { path, edits }) => self.edit_file(path, edits, deadline).await,
            Ok(ToolPlan::Search(plan)) => self.search(plan, deadline).await,
            Ok(ToolPlan::List { path }) => self.list_dir(path, deadline).await,
            Ok(ToolPlan::Diff) => self.workspace_diff(deadline).await,
        };
        // The call ended with its own run, which holds what it settled.
        self.take_settled_call();
        let mut run = run.bounded(self.limits.max_result_bytes);
        // A successful RPC does not mean the sandbox survived. The result
        // stays as it is, but the session cannot go on.
        if let Some(stop) = self.unusable() {
            run.stop = Some(stop);
        }
        run
    }

    /// Every host-side check of a tool call, before anything reaches kilnd.
    // A refusal is built once per call, on the host's cold path.
    #[allow(clippy::result_large_err)]
    fn plan(&self, tool: &str, arguments: Value) -> Result<ToolPlan, ToolRun> {
        match (self.tool_set, tool) {
            (_, "exec") => self.plan_exec(arguments).map(ToolPlan::Exec),
            (ToolSet::Base, "read_file") => {
                let args: ReadFileArgs = parse_arguments(arguments)?;
                let path = workspace_path(&args.path, false)
                    .map_err(|message| ToolRun::rejected("invalid_path", message))?;
                let max_bytes = self.read_max_bytes(args.max_bytes)?;
                Ok(ToolPlan::Read { path, max_bytes })
            }
            (ToolSet::CodingV1, "read_file") => self.plan_coding_read(arguments),
            (ToolSet::CodingV1, "edit_file") => self.plan_edit(arguments),
            (ToolSet::CodingV1, "search") => Self::plan_search(arguments).map(ToolPlan::Search),
            (_, "write_file") => {
                let args: WriteFileArgs = parse_arguments(arguments)?;
                let path = workspace_path(&args.path, false)
                    .map_err(|message| ToolRun::rejected("invalid_path", message))?;
                if args.content.len() > self.limits.max_file_bytes {
                    return Err(ToolRun::rejected(
                        "invalid_arguments",
                        "content exceeds the per-file limit",
                    ));
                }
                Ok(ToolPlan::Write {
                    path,
                    bytes: args.content.into_bytes(),
                })
            }
            (_, "list_dir") => {
                let args: ListDirArgs = parse_arguments(arguments)?;
                let path = workspace_path(&args.path, true)
                    .map_err(|message| ToolRun::rejected("invalid_path", message))?;
                Ok(ToolPlan::List { path })
            }
            (_, "workspace_diff") => {
                if serde_json::from_value::<WorkspaceDiffArgs>(arguments).is_err() {
                    return Err(ToolRun::rejected(
                        "invalid_arguments",
                        "workspace_diff accepts an empty object",
                    ));
                }
                Ok(ToolPlan::Diff)
            }
            _ => Err(ToolRun::rejected("unknown_tool", "tool is not registered")),
        }
    }

    /// `read_file`'s `max_bytes`, defaulting to the per-file limit.
    #[allow(clippy::result_large_err)]
    fn read_max_bytes(&self, max_bytes: Option<usize>) -> Result<usize, ToolRun> {
        let max_bytes = max_bytes.unwrap_or(self.limits.max_file_bytes);
        if max_bytes == 0 || max_bytes > self.limits.max_file_bytes {
            return Err(ToolRun::rejected(
                "invalid_arguments",
                "max_bytes is outside the per-file limit",
            ));
        }
        Ok(max_bytes)
    }

    #[allow(clippy::result_large_err)]
    fn plan_exec(&self, arguments: Value) -> Result<ExecPlan, ToolRun> {
        let args: ExecArgs = parse_arguments(arguments)?;
        if args.argv.is_empty() || args.argv.len() > 128 || args.argv[0].is_empty() {
            return Err(ToolRun::rejected(
                "invalid_arguments",
                "argv must contain 1..=128 entries and argv[0] must be nonempty",
            ));
        }
        if args.argv.iter().any(|arg| arg.contains('\0')) {
            return Err(ToolRun::rejected(
                "invalid_arguments",
                "argv may not contain NUL",
            ));
        }
        let argv_bytes = args
            .argv
            .iter()
            .try_fold(0usize, |total, arg| total.checked_add(arg.len() + 1));
        if argv_bytes.is_none_or(|bytes| bytes > self.limits.max_argv_bytes) {
            return Err(ToolRun::rejected(
                "invalid_arguments",
                "argv exceeds the harness byte limit",
            ));
        }
        if args.stdin.len() > self.limits.max_stdin_bytes {
            return Err(ToolRun::rejected(
                "invalid_arguments",
                "stdin exceeds the harness byte limit",
            ));
        }
        let timeout = match args.timeout_ms {
            Some(0) => {
                return Err(ToolRun::rejected(
                    "invalid_arguments",
                    "timeout_ms must be positive",
                ));
            }
            Some(ms) => Duration::from_millis(u64::from(ms)),
            None => self.limits.timeout,
        };
        if timeout > self.limits.timeout {
            return Err(ToolRun::rejected(
                "invalid_arguments",
                "timeout_ms exceeds the per-tool deadline",
            ));
        }
        Ok(ExecPlan {
            argv: args.argv,
            stdin: args.stdin.into_bytes(),
            timeout,
        })
    }

    async fn exec_tool(
        &self,
        plan: ExecPlan,
        deadline: Instant,
        call_id: &str,
        follow_deadline: Instant,
    ) -> ToolRun {
        if !self.exec_context.streams_exec() {
            return match self
                .run(plan.argv, plan.stdin, plan.timeout, deadline)
                .await
            {
                Ok(execution) => execution_run(&execution, self.limits.max_result_bytes / 3),
                Err(error) => tool_error_run(error, "kilnd_error", self.typed_reasons),
            };
        }
        let streamed = self
            .run_streamed(
                plan.argv,
                plan.stdin,
                plan.timeout,
                deadline,
                call_id,
                follow_deadline,
            )
            .await;
        let stopped = streamed.live.as_ref().and_then(|live| live.stopped.clone());
        let mut run = match streamed.execution {
            Ok(execution) => execution_run(&execution, self.limits.max_result_bytes / 3),
            Err(error) => tool_error_run(error, "kilnd_error", self.typed_reasons),
        };
        // The cutoff fired: the result says so, whatever kilnd's row did.
        if let (Some(stopped), Value::Object(result)) = (stopped, &mut run.result) {
            result.insert("stopped".to_string(), Value::String(stopped));
        }
        run.live = streamed.live.map(Box::new);
        run
    }

    async fn read_file(&self, path: String, max_bytes: usize, deadline: Instant) -> ToolRun {
        let stat = match self
            .run(
                vec![
                    "/bin/stat".to_string(),
                    "-c".to_string(),
                    "%s".to_string(),
                    "--".to_string(),
                    path.clone(),
                ],
                Vec::new(),
                self.limits.timeout,
                deadline,
            )
            .await
        {
            Ok(execution) => execution,
            Err(error) => return tool_error_run(error, "kilnd_error", self.typed_reasons),
        };
        if !stat.succeeded() {
            return execution_error(&stat, "stat_failed");
        }
        let Some(size) = std::str::from_utf8(&stat.stdout.data)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
        else {
            return execution_error(&stat, "invalid_stat_output");
        };
        let read = match self
            .run(
                vec![
                    "/usr/bin/head".to_string(),
                    "-c".to_string(),
                    max_bytes.to_string(),
                    "--".to_string(),
                    path,
                ],
                Vec::new(),
                self.limits.timeout,
                deadline,
            )
            .await
        {
            Ok(execution) => execution,
            Err(error) => return tool_error_run(error, "kilnd_error", self.typed_reasons),
        };
        if !read.succeeded() {
            return combine_execution_error(&stat, &read, "read_failed");
        }
        let utf8 = std::str::from_utf8(&read.stdout.data).is_ok();
        let content = String::from_utf8_lossy(&read.stdout.data);
        let mut run = ToolRun {
            ok: true,
            result: json!({
                "ok": true,
                "content": content,
                "utf8": utf8,
                "file_bytes": size,
                "returned_bytes": read.stdout.data.len(),
                "truncated": size > read.stdout.data.len() as u64
            }),
            execution_ids: vec![stat.id.clone(), read.id.clone()],
            stdout: merge_evidence(evidence(&stat.stdout), evidence(&read.stdout)),
            stderr: merge_evidence(evidence(&stat.stderr), evidence(&read.stderr)),
            stop: None,
            live: None,
            earlier: None,
        };
        if size > max_bytes as u64 {
            run.stdout.truncated = true;
        }
        run
    }

    async fn write_file(&self, path: String, bytes: Vec<u8>, deadline: Instant) -> ToolRun {
        let byte_count = bytes.len();
        match self
            .run(write_argv(&path), bytes, self.limits.timeout, deadline)
            .await
        {
            Ok(execution) if execution.succeeded() => ToolRun::from_execution(
                &execution,
                json!({"ok": true, "bytes_written": byte_count}),
                true,
            ),
            Ok(execution) => execution_error(&execution, "write_failed"),
            Err(error) => tool_error_run(error, "kilnd_error", self.typed_reasons),
        }
    }

    async fn list_dir(&self, path: String, deadline: Instant) -> ToolRun {
        match self
            .run(
                // Names only, one per line, directories marked with '/'.
                // Long listings carry timestamps and link counts that would
                // make otherwise identical sessions diverge on replay.
                vec![
                    "/bin/ls".to_string(),
                    "-1Ap".to_string(),
                    "--".to_string(),
                    path,
                ],
                Vec::new(),
                self.limits.timeout,
                deadline,
            )
            .await
        {
            Ok(execution) if execution.succeeded() => {
                let listing =
                    bounded_text(&execution.stdout.data, self.limits.max_result_bytes / 2);
                let mut run = ToolRun::from_execution(
                    &execution,
                    json!({
                        "ok": true,
                        "listing": listing.text,
                        "listing_truncated": listing.truncated
                    }),
                    true,
                );
                run.stdout.truncated |= listing.truncated;
                run
            }
            Ok(execution) => execution_error(&execution, "list_failed"),
            Err(error) => tool_error_run(error, "kilnd_error", self.typed_reasons),
        }
    }

    async fn workspace_diff(&self, deadline: Instant) -> ToolRun {
        match self.manifest(deadline).await {
            Ok(manifest) => {
                let added: Vec<_> = manifest.files.difference(&self.baseline).cloned().collect();
                let removed: Vec<_> = self.baseline.difference(&manifest.files).cloned().collect();
                ToolRun {
                    ok: true,
                    result: json!({"ok": true, "added": added, "removed": removed}),
                    execution_ids: manifest.execution_ids,
                    stdout: manifest.stdout,
                    stderr: manifest.stderr,
                    stop: None,
                    live: None,
                    earlier: None,
                }
            }
            Err(error) => tool_error_run(error, "workspace_diff_failed", self.typed_reasons),
        }
    }

    async fn manifest(&self, deadline: Instant) -> Result<Manifest, ToolError> {
        let execution = self
            .run(
                vec![
                    "/usr/bin/find".to_string(),
                    WORKSPACE.to_string(),
                    "-xdev".to_string(),
                    "-type".to_string(),
                    "f".to_string(),
                    "-print0".to_string(),
                ],
                Vec::new(),
                self.limits.timeout,
                deadline,
            )
            .await?;
        if !execution.succeeded() {
            return Err(manifest_error(
                &execution,
                format!(
                    "find state {:?}, exit {:?}",
                    execution.state, execution.exit_code
                ),
            ));
        }
        if execution.stdout.truncated
            || !execution.stdout.total_bytes_known
            || execution.stdout.data.len() > self.limits.max_manifest_bytes
        {
            return Err(manifest_error(
                &execution,
                "manifest output was truncated, unsettled, or oversized",
            ));
        }
        let mut files = BTreeSet::new();
        for raw in execution.stdout.data.split(|byte| *byte == 0) {
            if raw.is_empty() {
                continue;
            }
            let Ok(path) = std::str::from_utf8(raw) else {
                return Err(manifest_error(&execution, "non-UTF-8 path"));
            };
            let Some(relative) = path.strip_prefix("/workspace/") else {
                return Err(manifest_error(&execution, "path escaped /workspace"));
            };
            if relative.is_empty() || relative.chars().any(char::is_control) {
                return Err(manifest_error(&execution, "invalid path in manifest"));
            }
            files.insert(relative.to_string());
            if files.len() > self.limits.max_manifest_entries {
                return Err(manifest_error(&execution, "too many workspace files"));
            }
        }
        Ok(Manifest {
            files,
            execution_ids: vec![execution.id],
            stdout: evidence(&execution.stdout),
            stderr: evidence(&execution.stderr),
        })
    }

    /// The start of every Exec: refuse on a sandbox already unusable, and
    /// bound the call by the time left. Returns the RPC deadline and the
    /// request, whose command timeout leaves kilnd its settlement reserve.
    fn prepare_exec(
        &self,
        argv: Vec<String>,
        stdin: Vec<u8>,
        requested_timeout: Duration,
        deadline: Instant,
    ) -> Result<(Instant, SandboxExec), ToolError> {
        if self.unusable().is_some() {
            return Err(ToolError::Api(ApiError::new(
                tonic::Code::FailedPrecondition,
                "an earlier execution left the sandbox unusable",
            )));
        }
        let now = Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            return Err(ToolError::Deadline);
        }
        let wall_timeout = requested_timeout
            .min(remaining)
            .max(Duration::from_millis(1));
        let rpc_deadline = now + wall_timeout;
        let request = SandboxExec {
            sandbox_id: self.sandbox_id.clone(),
            argv,
            stdin,
            timeout: command_timeout(wall_timeout),
            cwd: self.exec_context.cwd().to_string(),
            env: self.exec_context.env(),
        };
        Ok((rpc_deadline, request))
    }

    /// Record that an Exec request is being sent now, and return when.
    fn note_sent(&self) -> Instant {
        let sent = Instant::now();
        *self
            .last_exec_sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sent);
        sent
    }

    /// End the session at this call with `message`, keeping the
    /// execution's ID as evidence and none of its output.
    fn refuse_execution(&self, execution_id: String, message: &str) -> ToolError {
        let stop = ToolStop::Unusable(message.to_string());
        self.unusable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert(stop.clone());
        ToolError::Unusable {
            stop,
            execution_ids: vec![execution_id],
        }
    }

    /// Accept an execution kilnd returned for a request sent at `sent`: it
    /// is activity, it must have run as sent, and one that left the
    /// sandbox unusable stops the session after this call. A streamed one,
    /// given the furthest offsets its live stream proved, must also pass
    /// [`streamed_row_problem`].
    fn settle(
        &self,
        execution: Execution,
        sent: Instant,
        streamed: Option<[u64; 2]>,
    ) -> Result<Execution, ToolError> {
        // A returned execution proves kilnd saw activity after the request
        // was sent, so the send time is the anchor, on the early side; while
        // it ran, kilnd paused idle. A failed RPC proves nothing: an
        // execution row found later by recovery is recorded then, and
        // otherwise the last known activity stands, so the next boundary
        // renews at once if a renew is overdue.
        self.record_activity(sent);
        // The command ran, but not as sent, for example on an older daemon
        // that ignored the options after a reconnect: keep its ID as
        // evidence, never its output, and stop at this call.
        // A streamed row is the result itself, so it must also say it was
        // streamed and hold the output it or the live stream showed: a
        // daemon that does not retain output (`--output-storage metadata`
        // or `none`) would make results silently empty.
        let problem = match streamed {
            Some(live_end) => streamed_row_problem(self.exec_context, &execution, live_end),
            None => (!self.exec_context.ran_as_sent(&execution)).then_some(EXEC_CONTEXT_MISMATCH),
        };
        if let Some(problem) = problem {
            return Err(self.refuse_execution(execution.id, problem));
        }
        if execution.sandbox_reusable == Some(false) {
            let mut unusable = self
                .unusable
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            unusable.get_or_insert(unusable_stop(execution.cancel_reason));
        }
        Ok(execution)
    }

    async fn run(
        &self,
        argv: Vec<String>,
        stdin: Vec<u8>,
        requested_timeout: Duration,
        deadline: Instant,
    ) -> Result<Execution, ToolError> {
        let (rpc_deadline, mut request) =
            self.prepare_exec(argv, stdin, requested_timeout, deadline)?;
        let mut raced = false;
        let (execution, sent) = loop {
            let sent = self.note_sent();
            match timeout_at(rpc_deadline, self.api.exec(request.clone())).await {
                Err(_) => return Err(ToolError::Deadline),
                // kilnd recorded nothing and the sandbox is READY again: send
                // the same request once more, within what is left of the
                // same budget.
                Ok(Err(error)) if !raced && execution_start_raced(&error, self.typed_reasons) => {
                    raced = true;
                    let left = rpc_deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(ToolError::Deadline);
                    }
                    request.timeout = command_timeout(left);
                }
                // A second race is an ordinary, recoverable failure: the
                // reason is dropped so nothing reads it as a terminal one.
                Ok(Err(error)) if execution_start_raced(&error, self.typed_reasons) => {
                    return Err(ToolError::Api(ApiError {
                        reason: None,
                        ..error
                    }));
                }
                Ok(Err(error)) => return Err(ToolError::Api(error)),
                Ok(Ok(execution)) => break (execution, sent),
            }
        };
        self.settle(execution, sent, None)
    }

    /// [`ToolSession::run`] through `ExecStream`, for the model's `exec` in
    /// a streaming context. Everything `run` does holds, and also: live
    /// bytes go to the session's sink under `call_id`; output past
    /// [`RUNAWAY_OUTPUT_BYTES`] on either stream drops the stream, so kilnd
    /// cancels the command; a stream that ends without its result after
    /// kilnd started the command is followed to kilnd's row until
    /// `follow_deadline`; one that breaks before is a lost response, left
    /// to the engine's recovery, which reads what this call learned from
    /// [`ToolSession::stream_call`]; and a refusal for lack of stream
    /// capacity is retried, never sent unary.
    async fn run_streamed(
        &self,
        argv: Vec<String>,
        stdin: Vec<u8>,
        requested_timeout: Duration,
        deadline: Instant,
        call_id: &str,
        follow_deadline: Instant,
    ) -> StreamedRun {
        *self
            .stream_call
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(StreamCall::default());
        let mut observer = CallObserver::new(call_id, self.live.clone(), &self.stream_call);
        let execution = self
            .stream_once(
                argv,
                stdin,
                requested_timeout,
                deadline,
                follow_deadline,
                &mut observer,
            )
            .await;
        let live = observer.evidence();
        observer.finish();
        StreamedRun { execution, live }
    }

    async fn stream_once(
        &self,
        argv: Vec<String>,
        stdin: Vec<u8>,
        requested_timeout: Duration,
        deadline: Instant,
        follow_deadline: Instant,
        observer: &mut CallObserver<'_>,
    ) -> Result<Execution, ToolError> {
        const FIRST_BUSY_DELAY: Duration = Duration::from_millis(50);
        const MAX_BUSY_DELAY: Duration = Duration::from_secs(1);
        let (rpc_deadline, mut request) =
            self.prepare_exec(argv, stdin, requested_timeout, deadline)?;
        let mut raced = false;
        let mut busy_delay = FIRST_BUSY_DELAY;
        let (streamed, sent) = loop {
            let sent = self.note_sent();
            match timeout_at(
                rpc_deadline,
                self.api.exec_stream(request.clone(), &mut *observer),
            )
            .await
            {
                Err(_) => return Err(ToolError::Deadline),
                // As for a unary Exec: nothing ran, so send it once more.
                Ok(Err(error)) if !raced && execution_start_raced(&error, self.typed_reasons) => {
                    raced = true;
                    let left = rpc_deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(ToolError::Deadline);
                    }
                    request.timeout = command_timeout(left);
                }
                Ok(Err(error)) if execution_start_raced(&error, self.typed_reasons) => {
                    return Err(ToolError::Api(ApiError {
                        reason: None,
                        ..error
                    }));
                }
                // Every stream permit is taken, and kilnd allocated nothing.
                // Wait for one within the same budget. A unary Exec would
                // get through, but results must not depend on contention.
                Ok(Err(error)) if exec_stream_limit(&error, self.typed_reasons) => {
                    let wake = Instant::now() + busy_delay;
                    if wake >= rpc_deadline {
                        return Err(ToolError::Api(ApiError::new(
                            tonic::Code::ResourceExhausted,
                            format!(
                                "kilnd's stream capacity was busy for the whole call: {}",
                                error.message
                            ),
                        )));
                    }
                    sleep_until(wake).await;
                    busy_delay = (busy_delay * 2).min(MAX_BUSY_DELAY);
                    request.timeout =
                        command_timeout(rpc_deadline.saturating_duration_since(Instant::now()));
                }
                Ok(Err(error)) => return Err(ToolError::Api(error)),
                Ok(Ok(streamed)) => break (streamed, sent),
            }
        };
        match streamed {
            StreamedExec::Finished {
                execution,
                stdout,
                stderr,
            } => {
                observer.finished_totals = Some((stdout, stderr));
                self.settle(execution, sent, Some(observer.observed_end))
            }
            // kilnd broke the protocol: nothing it sends for this command can
            // be trusted, its row included, so the session ends with the ID,
            // if one arrived, as evidence. Dropping the stream already made
            // kilnd cancel the command, and cleanup deletes the sandbox.
            // Without an ID the call has none, so response-loss recovery
            // still looks for the row; the stop stands either way.
            StreamedExec::Broken {
                execution_id,
                protocol_violation: true,
                ..
            } => {
                let stop = ToolStop::Unusable(STREAM_PROTOCOL_VIOLATION.to_string());
                self.unusable
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_or_insert(stop.clone());
                Err(ToolError::Unusable {
                    stop,
                    execution_ids: execution_id.into_iter().collect(),
                })
            }
            // The cutoff dropped the stream, or it ended without its result
            // after kilnd started the command: kilnd's row is the result,
            // exactly what `ExecFinished` would have carried. Reading it
            // here, rather than leaving it to response-loss recovery, gives
            // the model the same result however late the row settles
            // within the call's time and recovery window.
            StreamedExec::Stopped { execution_id }
            | StreamedExec::Broken {
                execution_id: Some(execution_id),
                protocol_violation: false,
                ..
            } => match follow_row(self.api, &execution_id, follow_deadline).await {
                Some(execution) if is_terminal(&execution) => {
                    self.settle(execution, sent, Some(observer.observed_end))
                }
                // The command's outcome, and whether the sandbox survived
                // it, is unknown: keep its ID and end the session.
                _ => Err(self.refuse_execution(execution_id, STREAM_RESULT_UNREADABLE)),
            },
            // As a lost unary response: no execution known, so the engine's
            // recovery looks for what kilnd recorded.
            StreamedExec::Broken {
                execution_id: None,
                error,
                protocol_violation: false,
            } => Err(ToolError::Api(error)),
        }
    }
}

/// Read `execution_id` until kilnd reports it terminal, backing off from
/// 10 ms to 200 ms, until `deadline`. Returns the last row read, terminal or
/// not, or `None` if every read failed.
async fn follow_row<A: SandboxApi>(
    api: &A,
    execution_id: &str,
    deadline: Instant,
) -> Option<Execution> {
    const FIRST_DELAY: Duration = Duration::from_millis(10);
    const MAX_DELAY: Duration = Duration::from_millis(200);
    let mut delay = FIRST_DELAY;
    let mut last = None;
    loop {
        if let Ok(Ok(execution)) = timeout_at(deadline, api.get_execution(execution_id)).await
            && execution.id == execution_id
        {
            let terminal = is_terminal(&execution);
            last = Some(execution);
            if terminal {
                return last;
            }
        }
        let wake = Instant::now() + delay;
        if wake >= deadline {
            return last;
        }
        sleep_until(wake).await;
        delay = (delay * 2).min(MAX_DELAY);
    }
}

/// Reserve a small part of an Exec's wall budget for kilnd to drain the
/// exec cgroup and publish its terminal row. If settlement exceeds the wall
/// budget, dropping the RPC requests cancellation and session cleanup
/// deletes the sandbox through a separate bounded lifecycle path.
fn command_timeout(budget: Duration) -> Duration {
    budget
        .saturating_sub(Duration::from_millis(250))
        .max(Duration::from_millis(1))
}

/// What a streamed `exec` produced: the execution as `run` would return it,
/// and, once kilnd started it, how its output was delivered live.
struct StreamedRun {
    execution: Result<Execution, ToolError>,
    live: Option<LiveEvidence>,
}

/// The harness's own observer of one streamed call: it forwards live
/// output to the host sink, counts what arrived, and breaks off once
/// either stream's output passes [`RUNAWAY_OUTPUT_BYTES`]. Dropped before
/// [`CallObserver::finish`], as when the call is canceled or its deadline
/// passes, it still tells the sink the call ended.
struct CallObserver<'c> {
    call_id: &'c str,
    sink: Option<Arc<dyn LiveOutput>>,
    /// Where the session keeps what this call learns, for recovery.
    record: &'c Mutex<Option<StreamCall>>,
    started: bool,
    finished: bool,
    /// Per stream (stdout, stderr): the furthest offset seen, bytes
    /// delivered, and bytes reported dropped.
    observed_end: [u64; 2],
    delivered: [u64; 2],
    dropped: [u64; 2],
    /// Set when the runaway limit fired.
    stopped: bool,
    /// kilnd's own totals, from `ExecFinished`.
    finished_totals: Option<(LiveDelivery, LiveDelivery)>,
}

impl<'c> CallObserver<'c> {
    fn new(
        call_id: &'c str,
        sink: Option<Arc<dyn LiveOutput>>,
        record: &'c Mutex<Option<StreamCall>>,
    ) -> Self {
        Self {
            call_id,
            sink,
            record,
            started: false,
            finished: false,
            observed_end: [0; 2],
            delivered: [0; 2],
            dropped: [0; 2],
            stopped: false,
            finished_totals: None,
        }
    }

    fn index(stream: LiveStream) -> usize {
        match stream {
            LiveStream::Stdout => 0,
            LiveStream::Stderr => 1,
        }
    }

    /// Write what this call learned where recovery reads it.
    fn remember(&self, update: impl FnOnce(&mut StreamCall)) {
        if let Some(call) = self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            update(call);
        }
    }

    fn observe(&mut self, stream: LiveStream, end: u64) -> ControlFlow<()> {
        let index = Self::index(stream);
        self.observed_end[index] = self.observed_end[index].max(end);
        let live_end = self.observed_end;
        self.remember(|call| call.live_end = live_end);
        if self.observed_end[index] > RUNAWAY_OUTPUT_BYTES {
            self.stopped = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    /// What the audit records: kilnd's totals when the stream finished,
    /// otherwise this observer's counts, with dropped bytes a lower bound.
    /// `None` when kilnd never started the execution.
    fn evidence(&self) -> Option<LiveEvidence> {
        if !self.started {
            return None;
        }
        let (stdout, stderr) = self.finished_totals.unwrap_or_else(|| {
            let counted = |index: usize| LiveDelivery {
                delivered_bytes: self.delivered[index],
                dropped_bytes: self.dropped[index],
                dropped_bytes_known: false,
            };
            (counted(0), counted(1))
        });
        Some(LiveEvidence {
            stdout,
            stderr,
            stopped: self.stopped.then(|| OUTPUT_LIMIT.to_string()),
        })
    }

    fn finish(&mut self) {
        if self.started && !self.finished {
            self.finished = true;
            if let Some(sink) = &self.sink {
                sink.finished(self.call_id, self.stopped.then_some(OUTPUT_LIMIT));
            }
        }
    }
}

impl Drop for CallObserver<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

impl StreamObserver for CallObserver<'_> {
    fn started(&mut self, execution_id: &str) {
        self.started = true;
        self.remember(|call| call.execution_id = Some(execution_id.to_string()));
        if let Some(sink) = &self.sink {
            sink.started(self.call_id, execution_id);
        }
    }

    fn chunk(&mut self, stream: LiveStream, offset: u64, data: &[u8]) -> ControlFlow<()> {
        let index = Self::index(stream);
        let length = u64::try_from(data.len()).unwrap_or(u64::MAX);
        self.delivered[index] = self.delivered[index].saturating_add(length);
        if let Some(sink) = &self.sink {
            sink.chunk(self.call_id, stream, data);
        }
        self.observe(stream, offset.saturating_add(length))
    }

    fn gap(
        &mut self,
        stream: LiveStream,
        offset: u64,
        dropped: u64,
        known: bool,
    ) -> ControlFlow<()> {
        let index = Self::index(stream);
        self.dropped[index] = self.dropped[index].saturating_add(dropped);
        if let Some(sink) = &self.sink {
            sink.gap(self.call_id, stream, dropped, known);
        }
        self.observe(stream, offset.saturating_add(dropped))
    }
}

impl CheckRun {
    /// A check that gave no verdict, with no execution or output.
    #[must_use]
    pub fn error(reason: impl Into<String>) -> Self {
        Self {
            outcome: CheckOutcome::Error {
                reason: reason.into(),
            },
            execution_id: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_evidence: unknown_evidence(),
            stderr_evidence: unknown_evidence(),
        }
    }
}

/// The fixed argv `write_file` and seeding write a file with: `dd` reads
/// the bytes from stdin, and the validated path is one argv entry.
fn write_argv(path: &str) -> Vec<String> {
    vec![
        "/bin/dd".to_string(),
        format!("of={path}"),
        "status=none".to_string(),
    ]
}

/// A tool call that passed every host-side check.
enum ToolPlan {
    Exec(ExecPlan),
    Read {
        path: String,
        max_bytes: usize,
    },
    /// A coding `read_file` with a line range.
    ReadLines {
        path: String,
        start: u32,
        count: u32,
        max_bytes: usize,
    },
    Write {
        path: String,
        bytes: Vec<u8>,
    },
    Edit {
        path: String,
        edits: Vec<EditArgs>,
    },
    Search(coding::SearchPlan),
    List {
        path: String,
    },
    Diff,
}

struct ExecPlan {
    argv: Vec<String>,
    stdin: Vec<u8>,
    timeout: Duration,
}

#[allow(clippy::result_large_err)]
fn parse_arguments<T: serde::de::DeserializeOwned>(arguments: Value) -> Result<T, ToolRun> {
    serde_json::from_value(arguments)
        .map_err(|error| ToolRun::rejected("invalid_arguments", error.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecArgs {
    argv: Vec<String>,
    #[serde(default)]
    stdin: String,
    timeout_ms: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    path: String,
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArgs {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListDirArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceDiffArgs {}

struct Manifest {
    files: BTreeSet<String>,
    execution_ids: Vec<String>,
    stdout: OutputEvidence,
    stderr: OutputEvidence,
}

/// How a session ends after an execution left the sandbox unusable: as an
/// expiry when kilnd's cancel reason proves one, and otherwise closed.
pub(crate) fn unusable_stop(cause: Option<CancelReason>) -> ToolStop {
    match cause {
        Some(CancelReason::SandboxDeleted) => ToolStop::Expired(ExpiryReason::SandboxDeleted),
        Some(CancelReason::SandboxExpiredLifetime) => {
            ToolStop::Expired(ExpiryReason::ExpiredLifetime)
        }
        _ => ToolStop::unusable(),
    }
}

fn manifest_error(execution: &Execution, message: impl Into<String>) -> ToolError {
    ToolError::Manifest {
        message: message.into(),
        execution_ids: vec![execution.id.clone()],
        stdout: evidence(&execution.stdout),
        stderr: evidence(&execution.stderr),
    }
}

/// Resolve a model-supplied file path to an absolute `/workspace` path.
///
/// Models write workspace paths both ways, so `/workspace` and
/// `/workspace/<rel>` are accepted as well as `<rel>`: the prefix is stripped
/// exactly once and the remainder must then pass the relative rules (no
/// empty, `.` or `..` component, no leading `/`). `/workspace` alone, like
/// `.`, names the workspace itself, which only `list_dir` accepts. Every
/// other absolute path, including `/workspacefoo` and `//workspace/x`, is
/// refused.
pub(crate) fn workspace_path(raw: &str, allow_root: bool) -> Result<String, String> {
    let relative = match raw.strip_prefix(WORKSPACE) {
        Some("" | "/") => ".",
        Some(rest) => match rest.strip_prefix('/') {
            Some(rest) => rest,
            // `/workspacefoo` is a different directory.
            None => raw,
        },
        None => raw,
    };
    if relative == "." {
        return if allow_root {
            Ok(WORKSPACE.to_string())
        } else {
            Err("path names the workspace directory, not a file".to_string())
        };
    }
    if relative.is_empty() || relative.len() > 1024 || relative.starts_with('/') {
        return Err(
            "path must be relative to /workspace or start with /workspace/, at most 1024 bytes"
                .to_string(),
        );
    }
    if relative.chars().any(char::is_control) {
        return Err("path may not contain control characters".to_string());
    }
    let components: Vec<_> = relative.split('/').collect();
    if components
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err("path components may not be empty, '.' or '..'".to_string());
    }
    Ok(format!("{WORKSPACE}/{relative}"))
}

fn execution_run(execution: &Execution, text_limit: usize) -> ToolRun {
    let ok = execution.succeeded();
    let state = state_name(execution.state);
    let stdout = bounded_text(&execution.stdout.data, text_limit);
    let stderr = bounded_text(&execution.stderr.data, text_limit);
    let mut run = ToolRun::from_execution(
        execution,
        json!({
            "ok": ok,
            "state": state,
            "exit_code": execution.exit_code,
            "failure_reason": execution.failure_reason,
            "stdout": stdout.text,
            "stdout_observed_bytes": execution.stdout.observed_bytes,
            "stdout_total_bytes_known": execution.stdout.total_bytes_known,
            "stdout_capture_truncated": execution.stdout.truncated,
            "stdout_model_truncated": stdout.truncated,
            "stdout_truncated": execution.stdout.truncated || stdout.truncated,
            "stderr": stderr.text,
            "stderr_observed_bytes": execution.stderr.observed_bytes,
            "stderr_total_bytes_known": execution.stderr.total_bytes_known,
            "stderr_capture_truncated": execution.stderr.truncated,
            "stderr_model_truncated": stderr.truncated,
            "stderr_truncated": execution.stderr.truncated || stderr.truncated
        }),
        ok,
    );
    run.stdout.truncated |= stdout.truncated;
    run.stderr.truncated |= stderr.truncated;
    run
}

fn execution_error(execution: &Execution, kind: &str) -> ToolRun {
    ToolRun::from_execution(
        execution,
        json!({
            "ok": false,
            "error": kind,
            "state": state_name(execution.state),
            "exit_code": execution.exit_code,
            "failure_reason": execution.failure_reason
        }),
        false,
    )
}

/// [`execution_error`] with the start of the command's stderr, at most
/// [`ERROR_STDERR_BYTES`] of it, for a coding tool's failed command.
fn execution_error_with_stderr(execution: &Execution, kind: &str) -> ToolRun {
    let mut run = execution_error(execution, kind);
    let stderr = bounded_text(&execution.stderr.data, ERROR_STDERR_BYTES);
    if let Value::Object(result) = &mut run.result {
        result.insert("stderr".to_string(), Value::String(stderr.text));
    }
    run.stderr.truncated |= stderr.truncated;
    run
}

/// Whether kilnd kept every byte the program wrote to this stream.
fn complete(output: &CapturedOutput) -> bool {
    output.total_bytes_known
        && !output.truncated
        && output.data.len() as u64 == output.observed_bytes
}

/// A host refusal after `executions` ran: `kind` and `message` as for a
/// refusal before any, with every execution's ID and evidence in order.
fn refused_after(executions: &[&Execution], kind: &str, message: String) -> ToolRun {
    let mut run = ToolRun::rejected(kind, message);
    with_executions(&mut run, executions);
    run
}

/// Put `executions` first in `run`'s IDs and evidence, in order, each ID
/// once.
fn with_executions(run: &mut ToolRun, executions: &[&Execution]) {
    let mut ids = Vec::new();
    for id in executions
        .iter()
        .map(|execution| &execution.id)
        .chain(&run.execution_ids)
    {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    run.execution_ids = ids;
    for execution in executions.iter().rev() {
        run.stdout = merge_evidence(evidence(&execution.stdout), run.stdout);
        run.stderr = merge_evidence(evidence(&execution.stderr), run.stderr);
    }
}

fn combine_execution_error(first: &Execution, second: &Execution, kind: &str) -> ToolRun {
    let mut run = execution_error(second, kind);
    run.execution_ids.insert(0, first.id.clone());
    run.stdout = merge_evidence(evidence(&first.stdout), run.stdout);
    run.stderr = merge_evidence(evidence(&first.stderr), run.stderr);
    run
}

impl<A: SandboxApi> ToolSession<'_, A> {
    /// Write every seed file in order with exactly the `write_file` argv,
    /// one Exec per file, after a fixed `mkdir -p -- <dir>` for each parent
    /// directory not made yet (the paths were validated, and each is one
    /// argv entry). Each Exec is bounded by the tool timeout and the session
    /// deadline. The first failure stops seeding.
    async fn seed(&self, files: &[SeedFile], deadline: Instant) -> Result<SeedEvidence, ToolError> {
        let mut evidence = SeedEvidence {
            files: 0,
            bytes: 0,
            execution_ids: Vec::new(),
        };
        let mut directories = BTreeSet::new();
        for file in files {
            let path = workspace_path(&file.path, false).map_err(|message| ToolError::Seed {
                message: format!("seed path {:?} is invalid: {message}", file.path),
                execution_ids: evidence.execution_ids.clone(),
            })?;
            if let Some((parent, _)) = file.path.rsplit_once('/')
                && !directories.contains(parent)
            {
                let argv = vec![
                    "/bin/mkdir".to_string(),
                    "-p".to_string(),
                    "--".to_string(),
                    format!("{WORKSPACE}/{parent}"),
                ];
                self.seed_exec(argv, Vec::new(), deadline, &mut evidence, &file.path)
                    .await?;
                let mut directory = parent;
                directories.insert(directory.to_string());
                while let Some((up, _)) = directory.rsplit_once('/') {
                    directories.insert(up.to_string());
                    directory = up;
                }
            }
            self.seed_exec(
                write_argv(&path),
                file.contents.clone(),
                deadline,
                &mut evidence,
                &file.path,
            )
            .await?;
            evidence.files += 1;
            evidence.bytes = evidence.bytes.saturating_add(file.contents.len() as u64);
        }
        Ok(evidence)
    }

    /// One seeding Exec, which must exit 0 and leave the sandbox usable.
    async fn seed_exec(
        &self,
        argv: Vec<String>,
        stdin: Vec<u8>,
        deadline: Instant,
        evidence: &mut SeedEvidence,
        path: &str,
    ) -> Result<(), ToolError> {
        let failed = |evidence: &SeedEvidence, detail: String| ToolError::Seed {
            message: format!("seed file {path:?}: {detail}"),
            execution_ids: evidence.execution_ids.clone(),
        };
        let execution = match self.run(argv, stdin, self.limits.timeout, deadline).await {
            Ok(execution) => execution,
            Err(ToolError::Deadline) => {
                return Err(failed(evidence, "the session deadline expired".to_string()));
            }
            // It ran, but not as sent: its ID is evidence.
            Err(ToolError::Unusable {
                stop,
                execution_ids,
            }) => {
                evidence.execution_ids.extend(execution_ids);
                return Err(failed(
                    evidence,
                    format!("the sandbox is no longer usable ({stop:?})"),
                ));
            }
            Err(ToolError::Api(error)) if sandbox_unusable_refusal(&error, self.typed_reasons) => {
                return Err(failed(
                    evidence,
                    format!("the sandbox is no longer usable ({error})"),
                ));
            }
            Err(error) => return Err(failed(evidence, error.to_string())),
        };
        evidence.execution_ids.push(execution.id.clone());
        if let Some(stop) = self.unusable() {
            return Err(failed(
                evidence,
                format!("the sandbox is no longer usable ({stop:?})"),
            ));
        }
        if !execution.succeeded() {
            return Err(failed(
                evidence,
                format!(
                    "execution {}, exit {:?}",
                    state_name(execution.state),
                    execution.exit_code
                ),
            ));
        }
        Ok(())
    }

    /// Run a task's check once: `/bin/sh -s` with the script on stdin, under
    /// the check's timeout less the settlement reserve every Exec keeps,
    /// capped by `deadline`. There is no response-loss recovery: an RPC
    /// error is reported as an error verdict.
    pub async fn run_check(&self, check: &TaskCheck, deadline: Instant) -> CheckRun {
        let argv = vec!["/bin/sh".to_string(), "-s".to_string()];
        let execution = match self
            .run(argv, check.script.clone(), check.timeout, deadline)
            .await
        {
            Ok(execution) => execution,
            Err(ToolError::Deadline) => return CheckRun::error("deadline"),
            // It ran, but not as sent: an error verdict that keeps its ID
            // and none of its output.
            Err(ToolError::Unusable {
                stop,
                execution_ids,
            }) => {
                return CheckRun {
                    execution_id: execution_ids.into_iter().next(),
                    ..CheckRun::error(match stop {
                        ToolStop::Unusable(message) => message,
                        other => format!("{other:?}"),
                    })
                };
            }
            Err(ToolError::Api(error)) if sandbox_unusable_refusal(&error, self.typed_reasons) => {
                return CheckRun::error(format!("{UNUSABLE_SANDBOX}: {error}"));
            }
            Err(error) => return CheckRun::error(error.to_string()),
        };
        let outcome = match (execution.state, execution.exit_code) {
            (ExecutionState::Exited, Some(0)) => CheckOutcome::Passed,
            (ExecutionState::Exited, Some(code)) => CheckOutcome::Failed {
                exit_code: Some(code),
                timed_out: false,
            },
            (ExecutionState::TimedOut, exit_code) => CheckOutcome::Failed {
                exit_code,
                timed_out: true,
            },
            (state, _) => CheckOutcome::Error {
                reason: format!("the check execution ended {}", state_name(state)),
            },
        };
        CheckRun {
            outcome,
            execution_id: Some(execution.id.clone()),
            stdout: check_output(&execution.stdout.data),
            stderr: check_output(&execution.stderr.data),
            stdout_evidence: evidence(&execution.stdout),
            stderr_evidence: evidence(&execution.stderr),
        }
    }
}

fn tool_error_run(error: ToolError, kind: &str, typed_reasons: bool) -> ToolRun {
    match error {
        ToolError::Deadline => ToolRun::interrupted(ToolStop::Deadline),
        ToolError::Api(error) => ToolRun {
            ok: false,
            result: json!({"ok": false, "error": kind, "message": error.to_string()}),
            execution_ids: Vec::new(),
            stdout: unknown_evidence(),
            stderr: unknown_evidence(),
            stop: ExpiryReason::of(&error, typed_reasons)
                .map(ToolStop::Expired)
                .or_else(|| {
                    sandbox_unusable_refusal(&error, typed_reasons).then(ToolStop::unusable)
                }),
            live: None,
            earlier: None,
        },
        ToolError::Manifest {
            message,
            execution_ids,
            stdout,
            stderr,
        } => ToolRun {
            ok: false,
            result: json!({"ok": false, "error": kind, "message": message}),
            execution_ids,
            stdout,
            stderr,
            stop: None,
            live: None,
            earlier: None,
        },
        // Only seeding returns this, and never through a tool.
        ToolError::Seed {
            message,
            execution_ids,
        } => ToolRun {
            ok: false,
            result: json!({"ok": false, "error": kind, "message": message}),
            execution_ids,
            stdout: unknown_evidence(),
            stderr: unknown_evidence(),
            stop: None,
            live: None,
            earlier: None,
        },
        // Only the baseline returns this; a tool's own check in `execute`
        // sets the stop.
        ToolError::Unusable {
            stop,
            execution_ids,
        } => ToolRun {
            ok: false,
            result: json!({
                "ok": false,
                "error": kind,
                "message": match &stop {
                    ToolStop::Unusable(message) => message.as_str(),
                    _ => UNUSABLE_SANDBOX,
                },
            }),
            execution_ids,
            stdout: unknown_evidence(),
            stderr: unknown_evidence(),
            stop: Some(stop),
            live: None,
            earlier: None,
        },
    }
}

fn state_name(state: ExecutionState) -> &'static str {
    match state {
        ExecutionState::Exited => "exited",
        ExecutionState::TimedOut => "timed_out",
        ExecutionState::Canceled => "canceled",
        ExecutionState::Failed => "failed",
        ExecutionState::Interrupted => "interrupted",
        ExecutionState::Unknown => "unknown",
    }
}

fn bounded_text(bytes: &[u8], limit: usize) -> BoundedText {
    let end = bytes.len().min(limit);
    BoundedText {
        text: String::from_utf8_lossy(&bytes[..end]).into_owned(),
        truncated: bytes.len() > end,
    }
}

/// One check output stream for the journal: decoded lossily first, then cut
/// to at most [`CHECK_OUTPUT_BYTES`] at a character boundary, so replacement
/// characters for invalid bytes cannot grow it past the bound.
fn check_output(bytes: &[u8]) -> String {
    // Each decoded character takes at least one input byte, and an input
    // character is at most four bytes, so the first `CHECK_OUTPUT_BYTES + 3`
    // input bytes decode to the same bounded prefix as the whole stream.
    let end = bytes.len().min(CHECK_OUTPUT_BYTES + 3);
    let mut text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    if text.len() > CHECK_OUTPUT_BYTES {
        let mut cut = CHECK_OUTPUT_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

struct BoundedText {
    text: String,
    truncated: bool,
}

fn evidence(output: &CapturedOutput) -> OutputEvidence {
    OutputEvidence {
        observed_bytes: output.observed_bytes,
        total_bytes_known: output.total_bytes_known,
        truncated: output.truncated,
    }
}

fn merge_evidence(left: OutputEvidence, right: OutputEvidence) -> OutputEvidence {
    OutputEvidence {
        observed_bytes: left.observed_bytes.saturating_add(right.observed_bytes),
        total_bytes_known: left.total_bytes_known && right.total_bytes_known,
        truncated: left.truncated || right.truncated,
    }
}

fn empty_evidence() -> OutputEvidence {
    OutputEvidence {
        observed_bytes: 0,
        total_bytes_known: true,
        truncated: false,
    }
}

fn unknown_evidence() -> OutputEvidence {
    OutputEvidence {
        observed_bytes: 0,
        total_bytes_known: false,
        truncated: false,
    }
}

/// Find executions kilnd assigned that the harness has not seen, after a
/// lost response. A row is evidence only once it is terminal: a RUNNING row
/// can still be canceled, retire the sandbox, or settle any other way, so
/// each unsettled row is followed with `GetExecution`. Recovery returns
/// early only when it has found at least one new row and every one is
/// terminal; an empty read proves nothing before the deadline, so otherwise
/// it runs to its last pass. A row still unsettled then (RUNNING,
/// `NOT_FOUND`, an error, or the wrong ID all count) is reported as such,
/// and so is a window in which no `GetSandbox` read succeeded at all, or
/// whose last read failed with nothing found. Recovery is best-effort: it
/// can stop a session early, but the final check before a completed session
/// is what proves the sandbox stayed usable. A cancellation that arrives
/// during it returns at once.
///
/// Polls back off from 10 ms to 200 ms. `GetSandbox` is read on the first
/// pass, on every pass while no new row is known, and then every fourth
/// pass; between those only the unsettled IDs are read. The last pass starts
/// `min(100 ms, window / 4)` before the deadline, so its reads have that
/// long to answer, and reads both. Every recovered row must show a live
/// session's context, `/workspace` and the fixed env overrides; any other
/// row is reported as mismatched. This helper is for live sessions only: a
/// replay of a journal recorded before the exec context ran with an empty
/// cwd and no overrides, and [`Harness::replay`](crate::engine::Harness::replay)
/// recovers its rows against that context itself.
pub async fn recover_execution_evidence<A: SandboxApi>(
    api: &A,
    sandbox_id: &str,
    known: &BTreeSet<String>,
    deadline: Instant,
    cancel: &CancellationToken,
) -> RecoveredExecutionEvidence {
    recover_execution_evidence_in(
        api,
        sandbox_id,
        ExecContext::WorkspaceV1,
        known,
        deadline,
        cancel,
        None,
    )
    .await
}

/// [`recover_execution_evidence`] for a session in `exec_context`: every
/// recovered row must show it.
///
/// After a streamed `exec`, `streamed` is what that call learned. With
/// the execution ID known, only that execution is followed, with
/// `GetExecution`, independently of `GetSandbox`: a started stream made
/// exactly one row. Never read as terminal in the window, it is reported
/// unsettled, under its ID. With the ID unknown, rows are found as for a
/// unary call, but each must also be read once with `GetExecution` after it
/// is terminal, since `GetSandbox` leaves output out; one never so read
/// counts as unsettled. Every terminal streamed row must pass the checks a
/// returned streamed row passes, or it is reported mismatched with the
/// reason. A cancellation that cuts recovery short reports a known ID
/// unsettled rather than nothing.
pub(crate) async fn recover_execution_evidence_in<A: SandboxApi>(
    api: &A,
    sandbox_id: &str,
    exec_context: ExecContext,
    known: &BTreeSet<String>,
    deadline: Instant,
    cancel: &CancellationToken,
    streamed: Option<StreamCall>,
) -> RecoveredExecutionEvidence {
    // A cancellation that cuts recovery short keeps what is already known:
    // the execution a stream named is evidence, its outcome unknown.
    let known_id = streamed.as_ref().and_then(|call| call.execution_id.clone());
    let recover = async {
        match streamed {
            Some(StreamCall {
                execution_id: Some(execution_id),
                live_end,
            }) => follow_recovered(api, exec_context, execution_id, live_end, deadline).await,
            streamed => {
                let live_end = streamed.map(|call| call.live_end);
                recover_until(api, sandbox_id, exec_context, known, deadline, live_end).await
            }
        }
    };
    // Recovery after a cancellation still runs its bounded window, to record
    // what kilnd assigned; a cancellation arriving during it returns at
    // once, and the caller's Canceled stop wins.
    if cancel.is_cancelled() {
        return recover.await;
    }
    tokio::select! {
        () = cancel.cancelled() => known_id.map_or_else(
            RecoveredExecutionEvidence::empty,
            RecoveredExecutionEvidence::unsettled,
        ),
        recovered = recover => recovered,
    }
}

/// Recovery of a streamed execution whose ID `ExecStarted` gave.
async fn follow_recovered<A: SandboxApi>(
    api: &A,
    exec_context: ExecContext,
    execution_id: String,
    live_end: [u64; 2],
    deadline: Instant,
) -> RecoveredExecutionEvidence {
    match follow_row(api, &execution_id, deadline).await {
        // Every row read here came from `GetExecution`.
        Some(row) => {
            let confirmed = BTreeSet::from([row.id.clone()]);
            aggregate_recovered(vec![row], exec_context, Some((live_end, &confirmed)))
        }
        None => RecoveredExecutionEvidence::unsettled(execution_id),
    }
}

/// `streamed` is set, to the live stream's furthest offsets, when the lost
/// call was a streamed `exec` whose execution ID never arrived.
async fn recover_until<A: SandboxApi>(
    api: &A,
    sandbox_id: &str,
    exec_context: ExecContext,
    known: &BTreeSet<String>,
    deadline: Instant,
    streamed: Option<[u64; 2]>,
) -> RecoveredExecutionEvidence {
    const FIRST_DELAY: Duration = Duration::from_millis(10);
    const MAX_DELAY: Duration = Duration::from_millis(200);
    const RESCAN_EVERY: u32 = 4;
    // The last pass starts this long before the deadline, so its reads have
    // time to answer.
    let window = deadline.saturating_duration_since(Instant::now());
    let final_reserve = Duration::from_millis(100).min(window / 4);
    let final_pass_at = deadline.checked_sub(final_reserve).unwrap_or(deadline);
    let mut rows: Vec<Execution> = Vec::new();
    // Streamed rows read with `GetExecution` once terminal.
    let mut confirmed = BTreeSet::new();
    let mut listed = false;
    let mut delay = FIRST_DELAY;
    let mut pass = 0u32;
    loop {
        let last = Instant::now() >= final_pass_at;
        let mut last_read = None;
        if last || rows.is_empty() || pass % RESCAN_EVERY == 0 {
            let read = rescan_recent(api, sandbox_id, known, deadline, &mut rows).await;
            listed |= read;
            last_read = Some(read);
        }
        let settled_row = |row: &Execution, confirmed: &BTreeSet<String>| {
            is_terminal(row) && (streamed.is_none() || confirmed.contains(&row.id))
        };
        for row in &mut rows {
            if settled_row(row, &confirmed) {
                continue;
            }
            if let Ok(Ok(execution)) = timeout_at(deadline, api.get_execution(&row.id)).await
                && execution.id == row.id
            {
                if is_terminal(&execution) {
                    confirmed.insert(execution.id.clone());
                }
                *row = execution;
            }
        }
        let settled = !rows.is_empty() && rows.iter().all(|row| settled_row(row, &confirmed));
        if settled || last {
            // An earlier empty read does not stand in for a last one that
            // failed: with nothing found and the last read lost, nothing is
            // known.
            let unreadable = !listed || (rows.is_empty() && last && last_read == Some(false));
            let streamed = streamed.map(|live_end| (live_end, &confirmed));
            let mut recovered = aggregate_recovered(rows, exec_context, streamed);
            recovered.unreadable = unreadable && !settled;
            return recovered;
        }
        pass = pass.saturating_add(1);
        let now = Instant::now();
        sleep_until((now + delay).min(final_pass_at.max(now))).await;
        delay = (delay * 2).min(MAX_DELAY);
    }
}

/// Read `GetSandbox` once and merge new rows: a row not seen before is
/// added, and an unsettled one is replaced by its newer view. Returns
/// whether the read succeeded.
async fn rescan_recent<A: SandboxApi>(
    api: &A,
    sandbox_id: &str,
    known: &BTreeSet<String>,
    deadline: Instant,
    rows: &mut Vec<Execution>,
) -> bool {
    let Ok(Ok(executions)) = timeout_at(deadline, api.recent_executions(sandbox_id)).await else {
        return false;
    };
    for execution in executions
        .into_iter()
        .filter(|execution| !known.contains(&execution.id))
    {
        match rows.iter_mut().find(|row| row.id == execution.id) {
            Some(row) if !is_terminal(row) => *row = execution,
            Some(_) => {}
            None => rows.push(execution),
        }
    }
    true
}

fn is_terminal(execution: &Execution) -> bool {
    execution.state != ExecutionState::Unknown
}

impl RecoveredExecutionEvidence {
    /// `execution_id` is known, its outcome not.
    fn unsettled(execution_id: String) -> Self {
        Self {
            execution_ids: vec![execution_id.clone()],
            stdout: unknown_evidence(),
            stderr: unknown_evidence(),
            unsettled: Some(execution_id),
            ..Self::empty()
        }
    }

    /// Nothing found, and nothing claimed.
    fn empty() -> Self {
        Self {
            execution_ids: Vec::new(),
            stdout: empty_evidence(),
            stderr: empty_evidence(),
            sandbox_reusable: None,
            cancel_reason: None,
            unsettled: None,
            unreadable: false,
            mismatched: None,
            mismatch: None,
        }
    }
}

/// The evidence of recovered `rows`. After a streamed call, `streamed` is
/// the live stream's furthest offsets and the rows read with `GetExecution`
/// once terminal: only those are settled, and each must pass
/// [`streamed_row_problem`].
fn aggregate_recovered(
    rows: Vec<Execution>,
    exec_context: ExecContext,
    streamed: Option<([u64; 2], &BTreeSet<String>)>,
) -> RecoveredExecutionEvidence {
    let mut recovered = RecoveredExecutionEvidence::empty();
    for execution in rows {
        let confirmed = streamed.is_none_or(|(_, confirmed)| confirmed.contains(&execution.id));
        let settled = is_terminal(&execution) && confirmed;
        // Every execution of this sandbox is this session's, so each must
        // show what the session sends, settled or not; a settled streamed
        // one must pass every check of a returned one.
        let problem = match streamed {
            Some((live_end, _)) if settled => {
                streamed_row_problem(exec_context, &execution, live_end)
            }
            _ => (!exec_context.ran_as_sent(&execution)).then_some(EXEC_CONTEXT_MISMATCH),
        };
        if let Some(problem) = problem
            && recovered.mismatched.is_none()
        {
            recovered.mismatched = Some(execution.id.clone());
            recovered.mismatch = Some(problem);
        }
        if !settled {
            recovered
                .unsettled
                .get_or_insert_with(|| execution.id.clone());
        } else if recovered.sandbox_reusable != Some(false) {
            if execution.sandbox_reusable == Some(false) {
                recovered.cancel_reason = execution.cancel_reason;
            }
            if execution.sandbox_reusable.is_some() {
                recovered.sandbox_reusable = execution.sandbox_reusable;
            }
        }
        recovered.stdout = merge_evidence(recovered.stdout, evidence(&execution.stdout));
        recovered.stderr = merge_evidence(recovered.stderr, evidence(&execution.stderr));
        recovered.execution_ids.push(execution.id);
    }
    recovered
}

/// The registered tools of `tool_set`; `streaming` describes `exec` as a
/// streaming session runs it. Only the coding set's descriptions use
/// `limits`.
fn definitions(streaming: bool, tool_set: ToolSet, limits: &ToolLimits) -> Vec<ToolDefinition> {
    let mut tools = base_definitions(streaming);
    if tool_set == ToolSet::CodingV1 {
        coding::extend_definitions(&mut tools, limits);
    }
    tools
}

/// The base tools; `streaming` describes `exec` as a streaming session
/// runs it.
fn base_definitions(streaming: bool) -> Vec<ToolDefinition> {
    let mut exec =
        "Execute literal argv in the session sandbox; no host shell is used.".to_string();
    if streaming {
        exec.push_str(EXEC_STREAM_DESCRIPTION);
    }
    vec![
        ToolDefinition {
            name: "exec".to_string(),
            description: exec,
            input_schema: json!({
                "type": "object",
                "required": ["argv"],
                "additionalProperties": false,
                "properties": {
                    "argv": {"type": "array", "minItems": 1, "maxItems": 128, "items": {"type": "string"}},
                    "stdin": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 1}
                }
            }),
        },
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a bounded prefix of one /workspace file, given as a relative path \
                          (notes.txt) or under /workspace/ (/workspace/notes.txt)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "additionalProperties": false,
                "properties": {
                    "path": {"type": "string"},
                    "max_bytes": {"type": "integer", "minimum": 1}
                }
            }),
        },
        ToolDefinition {
            name: "write_file".to_string(),
            description: "Overwrite one /workspace file with bounded UTF-8 content; the path is \
                 relative (notes.txt) or under /workspace/ (/workspace/notes.txt)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["path", "content"],
                "additionalProperties": false,
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                }
            }),
        },
        ToolDefinition {
            name: "list_dir".to_string(),
            description: "List entry names in one /workspace directory: '.' or \
                          /workspace for the workspace itself, otherwise a relative path or \
                          one under /workspace/. Directory names end in '/'."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "additionalProperties": false,
                "properties": {"path": {"type": "string"}}
            }),
        },
        ToolDefinition {
            name: "workspace_diff".to_string(),
            description: "Compare the current /workspace file set with the session baseline."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::workspace_path;

    #[test]
    fn workspace_prefixed_paths_resolve_like_relative_ones() {
        for (raw, expected) in [
            ("fib.sh", "/workspace/fib.sh"),
            ("/workspace/fib.sh", "/workspace/fib.sh"),
            ("src/main.rs", "/workspace/src/main.rs"),
            ("/workspace/src/main.rs", "/workspace/src/main.rs"),
            ("/workspace/workspace/x", "/workspace/workspace/x"),
        ] {
            assert_eq!(workspace_path(raw, false).as_deref(), Ok(expected), "{raw}");
            assert_eq!(workspace_path(raw, true).as_deref(), Ok(expected), "{raw}");
        }
    }

    #[test]
    fn the_workspace_itself_is_a_directory_only() {
        for raw in [".", "/workspace", "/workspace/", "/workspace/."] {
            assert_eq!(
                workspace_path(raw, true).as_deref(),
                Ok("/workspace"),
                "{raw}"
            );
            assert!(workspace_path(raw, false).is_err(), "{raw}");
        }
    }

    #[test]
    fn other_paths_stay_invalid() {
        for raw in [
            "",
            "/",
            "/etc/passwd",
            "/workspace/../etc",
            "/workspace/..",
            "/workspace/./x",
            "/workspace//x",
            "/workspace/x/",
            "/workspacefoo",
            "/workspacefoo/x",
            "//workspace/x",
            "/tmp/workspace/x",
            "workspace/../x",
            "../x",
            "a//b",
            "./x",
            "x/\u{7}",
        ] {
            assert!(workspace_path(raw, false).is_err(), "{raw:?}");
            assert!(workspace_path(raw, true).is_err(), "{raw:?}");
        }
        let long = format!("/workspace/{}", "a".repeat(1025));
        assert!(workspace_path(&long, false).is_err());
        let fits = format!("/workspace/{}", "a".repeat(1024));
        assert!(workspace_path(&fits, false).is_ok());
    }
}
