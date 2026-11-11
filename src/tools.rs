use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::time::{Instant, sleep_until, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::api::{
    ApiError, CancelReason, CapturedOutput, Execution, ExecutionState, ExpiryReason, SandboxApi,
    SandboxExec, UnusableReason,
};
use crate::audit::{ContentSummary, OutputEvidence};
use crate::engine::{CheckOutcome, SeedFile, TaskCheck};
use crate::journal::SessionLimits;
use crate::provider::{ToolCall, ToolDefinition};

const WORKSPACE: &str = "/workspace";
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

#[derive(Debug, Clone)]
pub struct ToolRegistry {
    limits: ToolLimits,
    definitions: Vec<ToolDefinition>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            limits,
            definitions: definitions(),
        }
    }

    #[must_use]
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    /// Capture the fixed workspace baseline for a newly created sandbox.
    /// `ready_at` is a moment no later than kilnd made it ready, such as when
    /// create was sent: the first activity anchor, on the early side.
    /// `typed_reasons` says whether the daemon lists typed refusal reasons;
    /// only then does a tool call end on a refusal proving the sandbox gone.
    ///
    /// # Errors
    ///
    /// Returns an error if kilnd cannot execute or settle the bounded manifest.
    pub async fn start_session<'a, A: SandboxApi>(
        &'a self,
        api: &'a A,
        sandbox_id: String,
        session_deadline: Instant,
        typed_reasons: bool,
        ready_at: Instant,
        seed: &[SeedFile],
    ) -> Result<(ToolSession<'a, A>, Option<SeedEvidence>, BaselineEvidence), ToolError> {
        let mut session = ToolSession {
            api,
            sandbox_id,
            limits: &self.limits,
            baseline: BTreeSet::new(),
            typed_reasons,
            // The sandbox has just become ready.
            activity: Mutex::new((ready_at, 0)),
            last_exec_sent: Mutex::new(None),
            unusable: Mutex::new(None),
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolRun {
    pub ok: bool,
    pub result: Value,
    pub execution_ids: Vec<String>,
    pub stdout: OutputEvidence,
    pub stderr: OutputEvidence,
    pub stop: Option<ToolStop>,
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
}

impl RecoveredExecutionEvidence {
    /// How the session must end when a recovered row left the sandbox
    /// unusable, exactly as for a directly returned one.
    #[must_use]
    pub fn stop(&self) -> Option<ToolStop> {
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
        }
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

    /// Attach evidence recovered after a lost response. A recovered row that
    /// left the sandbox unusable stops the session like a returned one; an
    /// interruption already recorded keeps its own stop.
    pub fn attach_recovered(&mut self, recovered: RecoveredExecutionEvidence) {
        // A stop needs no IDs: an unreadable window has none.
        if self.stop.is_none() {
            self.stop = recovered.stop();
        }
        if recovered.execution_ids.is_empty() {
            return;
        }
        self.execution_ids = recovered.execution_ids;
        self.stdout = recovered.stdout;
        self.stderr = recovered.stderr;
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
        let recovered =
            recover_execution_evidence(self.api, &self.sandbox_id, known, deadline, cancel).await;
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
        let run = match self.plan(tool, arguments) {
            Err(rejected) => rejected,
            Ok(ToolPlan::Exec(plan)) => self.exec_tool(plan, deadline).await,
            Ok(ToolPlan::Read { path, max_bytes }) => {
                self.read_file(path, max_bytes, deadline).await
            }
            Ok(ToolPlan::Write { path, bytes }) => self.write_file(path, bytes, deadline).await,
            Ok(ToolPlan::List { path }) => self.list_dir(path, deadline).await,
            Ok(ToolPlan::Diff) => self.workspace_diff(deadline).await,
        };
        let mut run = run.bounded(self.limits.max_result_bytes);
        // A successful RPC does not mean the sandbox survived. The result
        // stays as it is, but the session cannot go on.
        if let Some(stop) = self.unusable() {
            run.stop = Some(stop);
        }
        run
    }

    /// Every host-side check of a tool call, before anything reaches kilnd.
    fn plan(&self, tool: &str, arguments: Value) -> Result<ToolPlan, ToolRun> {
        match tool {
            "exec" => self.plan_exec(arguments).map(ToolPlan::Exec),
            "read_file" => {
                let args: ReadFileArgs = parse_arguments(arguments)?;
                let path = workspace_path(&args.path, false)
                    .map_err(|message| ToolRun::rejected("invalid_path", message))?;
                let max_bytes = args.max_bytes.unwrap_or(self.limits.max_file_bytes);
                if max_bytes == 0 || max_bytes > self.limits.max_file_bytes {
                    return Err(ToolRun::rejected(
                        "invalid_arguments",
                        "max_bytes is outside the per-file limit",
                    ));
                }
                Ok(ToolPlan::Read { path, max_bytes })
            }
            "write_file" => {
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
            "list_dir" => {
                let args: ListDirArgs = parse_arguments(arguments)?;
                let path = workspace_path(&args.path, true)
                    .map_err(|message| ToolRun::rejected("invalid_path", message))?;
                Ok(ToolPlan::List { path })
            }
            "workspace_diff" => {
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

    async fn exec_tool(&self, plan: ExecPlan, deadline: Instant) -> ToolRun {
        match self
            .run(plan.argv, plan.stdin, plan.timeout, deadline)
            .await
        {
            Ok(execution) => execution_run(&execution, self.limits.max_result_bytes / 3),
            Err(error) => tool_error_run(error, "kilnd_error", self.typed_reasons),
        }
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

    async fn run(
        &self,
        argv: Vec<String>,
        stdin: Vec<u8>,
        requested_timeout: Duration,
        deadline: Instant,
    ) -> Result<Execution, ToolError> {
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
        // Reserve a small part of the wall budget for kilnd to drain the exec
        // cgroup and publish its terminal row. If settlement exceeds the wall
        // budget, dropping the RPC requests cancellation and session cleanup
        // deletes the sandbox through a separate bounded lifecycle path.
        let command_timeout = wall_timeout
            .saturating_sub(Duration::from_millis(250))
            .max(Duration::from_millis(1));
        let sent = Instant::now();
        *self
            .last_exec_sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sent);
        let result = timeout_at(
            rpc_deadline,
            self.api.exec(SandboxExec {
                sandbox_id: self.sandbox_id.clone(),
                argv,
                stdin,
                timeout: command_timeout,
            }),
        )
        .await;
        let execution = match result {
            Err(_) => return Err(ToolError::Deadline),
            Ok(Err(error)) => {
                return Err(ToolError::Api(
                    self.settle_not_ready(error, rpc_deadline).await,
                ));
            }
            Ok(Ok(execution)) => execution,
        };
        // A returned execution proves kilnd saw activity after the request
        // was sent, so the send time is the anchor, on the early side; while
        // it ran, kilnd paused idle. A failed RPC proves nothing: an
        // execution row found later by recovery is recorded then, and
        // otherwise the last known activity stands, so the next boundary
        // renews at once if a renew is overdue.
        self.record_activity(sent);
        if execution.sandbox_reusable == Some(false) {
            let mut unusable = self
                .unusable
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            unusable.get_or_insert(unusable_stop(execution.cancel_reason));
        }
        Ok(execution)
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
    Read { path: String, max_bytes: usize },
    Write { path: String, bytes: Vec<u8> },
    List { path: String },
    Diff,
}

struct ExecPlan {
    argv: Vec<String>,
    stdin: Vec<u8>,
    timeout: Duration,
}

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

    /// kilnd also sends `sandbox_not_ready` for a READY sandbox when an
    /// Exec lost a race with its own row insert, and says to retry. Ask once,
    /// within the call's deadline: only a sandbox that reads READY turns the
    /// refusal into an ordinary, recoverable error (its reason is dropped,
    /// its message kept). Anything else, a lookup failure included, stays
    /// terminal.
    async fn settle_not_ready(&self, error: ApiError, deadline: Instant) -> ApiError {
        if UnusableReason::of(&error, self.typed_reasons) != Some(UnusableReason::NotReady) {
            return error;
        }
        match timeout_at(deadline, self.api.sandbox_ready(&self.sandbox_id)).await {
            Ok(Ok(true)) => ApiError {
                reason: None,
                ..error
            },
            _ => error,
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
                    UnusableReason::of(&error, typed_reasons).map(|_| ToolStop::unusable())
                }),
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
        },
        // Only the baseline returns this; a tool's own check in `execute`
        // sets the stop.
        ToolError::Unusable {
            stop,
            execution_ids,
        } => ToolRun {
            ok: false,
            result: json!({"ok": false, "error": kind, "message": UNUSABLE_SANDBOX}),
            execution_ids,
            stdout: unknown_evidence(),
            stderr: unknown_evidence(),
            stop: Some(stop),
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
/// long to answer, and reads both.
pub async fn recover_execution_evidence<A: SandboxApi>(
    api: &A,
    sandbox_id: &str,
    known: &BTreeSet<String>,
    deadline: Instant,
    cancel: &CancellationToken,
) -> RecoveredExecutionEvidence {
    // Recovery after a cancellation still runs its bounded window, to record
    // what kilnd assigned; a cancellation arriving during it returns at
    // once, and the caller's Canceled stop wins.
    if cancel.is_cancelled() {
        return recover_until(api, sandbox_id, known, deadline).await;
    }
    tokio::select! {
        () = cancel.cancelled() => RecoveredExecutionEvidence::empty(),
        recovered = recover_until(api, sandbox_id, known, deadline) => recovered,
    }
}

async fn recover_until<A: SandboxApi>(
    api: &A,
    sandbox_id: &str,
    known: &BTreeSet<String>,
    deadline: Instant,
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
        for row in rows.iter_mut().filter(|row| !is_terminal(row)) {
            if let Ok(Ok(execution)) = timeout_at(deadline, api.get_execution(&row.id)).await
                && execution.id == row.id
            {
                *row = execution;
            }
        }
        let settled = !rows.is_empty() && rows.iter().all(is_terminal);
        if settled || last {
            // An earlier empty read does not stand in for a last one that
            // failed: with nothing found and the last read lost, nothing is
            // known.
            let unreadable = !listed || (rows.is_empty() && last && last_read == Some(false));
            let mut recovered = aggregate_recovered(rows);
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
        }
    }
}

fn aggregate_recovered(rows: Vec<Execution>) -> RecoveredExecutionEvidence {
    let mut recovered = RecoveredExecutionEvidence::empty();
    for execution in rows {
        if !is_terminal(&execution) {
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

fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "exec".to_string(),
            description: "Execute literal argv in the session sandbox; no host shell is used."
                .to_string(),
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
