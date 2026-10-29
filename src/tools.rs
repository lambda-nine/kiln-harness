use std::collections::BTreeSet;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::time::{Instant, sleep, timeout_at};

use crate::api::{ApiError, CapturedOutput, Execution, ExecutionState, SandboxApi, SandboxExec};
use crate::audit::{ContentSummary, OutputEvidence};
use crate::provider::ToolDefinition;

const WORKSPACE: &str = "/workspace";

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
    ///
    /// # Errors
    ///
    /// Returns an error if kilnd cannot execute or settle the bounded manifest.
    pub async fn start_session<'a, A: SandboxApi>(
        &'a self,
        api: &'a A,
        sandbox_id: String,
        session_deadline: Instant,
    ) -> Result<(ToolSession<'a, A>, BaselineEvidence), ToolError> {
        let mut session = ToolSession {
            api,
            sandbox_id,
            limits: &self.limits,
            baseline: BTreeSet::new(),
        };
        let manifest = session.manifest(session_deadline).await?;
        let evidence = BaselineEvidence {
            execution_ids: manifest.execution_ids,
            file_count: manifest.files.len(),
        };
        session.baseline = manifest.files;
        Ok((session, evidence))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineEvidence {
    pub execution_ids: Vec<String>,
    pub file_count: usize,
}

pub struct ToolSession<'a, A: SandboxApi> {
    api: &'a A,
    sandbox_id: String,
    limits: &'a ToolLimits,
    baseline: BTreeSet<String>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStop {
    Deadline,
    Canceled,
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

    pub fn attach_recovered(&mut self, recovered: RecoveredExecutionEvidence) {
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
            Self::Api(_) | Self::Deadline => (Vec::new(), unknown_evidence(), unknown_evidence()),
        }
    }
}

impl<A: SandboxApi> ToolSession<'_, A> {
    pub async fn recover_new_execution_evidence(
        &self,
        known: &BTreeSet<String>,
        deadline: Instant,
    ) -> RecoveredExecutionEvidence {
        recover_execution_evidence(self.api, &self.sandbox_id, known, deadline).await
    }

    pub async fn execute(
        &self,
        tool: &str,
        arguments: Value,
        session_deadline: Instant,
    ) -> ToolRun {
        let run = match tool {
            "exec" => self.exec_tool(arguments, session_deadline).await,
            "read_file" => self.read_file(arguments, session_deadline).await,
            "write_file" => self.write_file(arguments, session_deadline).await,
            "list_dir" => self.list_dir(arguments, session_deadline).await,
            "workspace_diff" => self.workspace_diff(arguments, session_deadline).await,
            _ => ToolRun::rejected("unknown_tool", "tool is not registered"),
        };
        run.bounded(self.limits.max_result_bytes)
    }

    async fn exec_tool(&self, arguments: Value, deadline: Instant) -> ToolRun {
        let args: ExecArgs = match serde_json::from_value(arguments) {
            Ok(args) => args,
            Err(error) => return ToolRun::rejected("invalid_arguments", error.to_string()),
        };
        if args.argv.is_empty() || args.argv.len() > 128 || args.argv[0].is_empty() {
            return ToolRun::rejected(
                "invalid_arguments",
                "argv must contain 1..=128 entries and argv[0] must be nonempty",
            );
        }
        if args.argv.iter().any(|arg| arg.contains('\0')) {
            return ToolRun::rejected("invalid_arguments", "argv may not contain NUL");
        }
        let argv_bytes = args
            .argv
            .iter()
            .try_fold(0usize, |total, arg| total.checked_add(arg.len() + 1));
        if argv_bytes.is_none_or(|bytes| bytes > self.limits.max_argv_bytes) {
            return ToolRun::rejected("invalid_arguments", "argv exceeds the harness byte limit");
        }
        if args.stdin.len() > self.limits.max_stdin_bytes {
            return ToolRun::rejected("invalid_arguments", "stdin exceeds the harness byte limit");
        }
        let requested = match args.timeout_ms {
            Some(0) => {
                return ToolRun::rejected("invalid_arguments", "timeout_ms must be positive");
            }
            Some(ms) => Duration::from_millis(u64::from(ms)),
            None => self.limits.timeout,
        };
        if requested > self.limits.timeout {
            return ToolRun::rejected(
                "invalid_arguments",
                "timeout_ms exceeds the per-tool deadline",
            );
        }
        match self
            .run(args.argv, args.stdin.into_bytes(), requested, deadline)
            .await
        {
            Ok(execution) => execution_run(&execution, self.limits.max_result_bytes / 3),
            Err(error) => tool_error_run(error, "kilnd_error"),
        }
    }

    async fn read_file(&self, arguments: Value, deadline: Instant) -> ToolRun {
        let args: ReadFileArgs = match serde_json::from_value(arguments) {
            Ok(args) => args,
            Err(error) => return ToolRun::rejected("invalid_arguments", error.to_string()),
        };
        let path = match workspace_path(&args.path, false) {
            Ok(path) => path,
            Err(message) => return ToolRun::rejected("invalid_path", message),
        };
        let max_bytes = args.max_bytes.unwrap_or(self.limits.max_file_bytes);
        if max_bytes == 0 || max_bytes > self.limits.max_file_bytes {
            return ToolRun::rejected(
                "invalid_arguments",
                "max_bytes is outside the per-file limit",
            );
        }

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
            Err(error) => return tool_error_run(error, "kilnd_error"),
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
            Err(error) => return tool_error_run(error, "kilnd_error"),
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

    async fn write_file(&self, arguments: Value, deadline: Instant) -> ToolRun {
        let args: WriteFileArgs = match serde_json::from_value(arguments) {
            Ok(args) => args,
            Err(error) => return ToolRun::rejected("invalid_arguments", error.to_string()),
        };
        let path = match workspace_path(&args.path, false) {
            Ok(path) => path,
            Err(message) => return ToolRun::rejected("invalid_path", message),
        };
        if args.content.len() > self.limits.max_file_bytes {
            return ToolRun::rejected("invalid_arguments", "content exceeds the per-file limit");
        }
        let bytes = args.content.into_bytes();
        let byte_count = bytes.len();
        match self
            .run(
                vec![
                    "/bin/dd".to_string(),
                    format!("of={path}"),
                    "status=none".to_string(),
                ],
                bytes,
                self.limits.timeout,
                deadline,
            )
            .await
        {
            Ok(execution) if execution.succeeded() => ToolRun::from_execution(
                &execution,
                json!({"ok": true, "bytes_written": byte_count}),
                true,
            ),
            Ok(execution) => execution_error(&execution, "write_failed"),
            Err(error) => tool_error_run(error, "kilnd_error"),
        }
    }

    async fn list_dir(&self, arguments: Value, deadline: Instant) -> ToolRun {
        let args: ListDirArgs = match serde_json::from_value(arguments) {
            Ok(args) => args,
            Err(error) => return ToolRun::rejected("invalid_arguments", error.to_string()),
        };
        let path = match workspace_path(&args.path, true) {
            Ok(path) => path,
            Err(message) => return ToolRun::rejected("invalid_path", message),
        };
        match self
            .run(
                vec![
                    "/bin/ls".to_string(),
                    "-la".to_string(),
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
            Err(error) => tool_error_run(error, "kilnd_error"),
        }
    }

    async fn workspace_diff(&self, arguments: Value, deadline: Instant) -> ToolRun {
        if serde_json::from_value::<WorkspaceDiffArgs>(arguments).is_err() {
            return ToolRun::rejected(
                "invalid_arguments",
                "workspace_diff accepts an empty object",
            );
        }
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
            Err(error) => tool_error_run(error, "workspace_diff_failed"),
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
        timeout_at(
            rpc_deadline,
            self.api.exec(SandboxExec {
                sandbox_id: self.sandbox_id.clone(),
                argv,
                stdin,
                timeout: command_timeout,
            }),
        )
        .await
        .map_err(|_| ToolError::Deadline)?
        .map_err(ToolError::Api)
    }
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

fn manifest_error(execution: &Execution, message: impl Into<String>) -> ToolError {
    ToolError::Manifest {
        message: message.into(),
        execution_ids: vec![execution.id.clone()],
        stdout: evidence(&execution.stdout),
        stderr: evidence(&execution.stderr),
    }
}

fn workspace_path(raw: &str, allow_root: bool) -> Result<String, String> {
    if raw == "." && allow_root {
        return Ok(WORKSPACE.to_string());
    }
    if raw.is_empty() || raw.len() > 1024 || raw.starts_with('/') {
        return Err("path must be a relative workspace path of at most 1024 bytes".to_string());
    }
    if raw.chars().any(char::is_control) {
        return Err("path may not contain control characters".to_string());
    }
    let components: Vec<_> = raw.split('/').collect();
    if components
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err("path components may not be empty, '.' or '..'".to_string());
    }
    Ok(format!("{WORKSPACE}/{raw}"))
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

fn tool_error_run(error: ToolError, kind: &str) -> ToolRun {
    match error {
        ToolError::Deadline => ToolRun::interrupted(ToolStop::Deadline),
        ToolError::Api(error) => ToolRun {
            ok: false,
            result: json!({"ok": false, "error": kind, "message": error.to_string()}),
            execution_ids: Vec::new(),
            stdout: unknown_evidence(),
            stderr: unknown_evidence(),
            stop: None,
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

pub async fn recover_execution_evidence<A: SandboxApi>(
    api: &A,
    sandbox_id: &str,
    known: &BTreeSet<String>,
    deadline: Instant,
) -> RecoveredExecutionEvidence {
    loop {
        let executions = match timeout_at(deadline, api.recent_executions(sandbox_id)).await {
            Ok(Ok(executions)) => executions,
            Ok(Err(_)) | Err(_) => Vec::new(),
        };
        let mut recovered = RecoveredExecutionEvidence {
            execution_ids: Vec::new(),
            stdout: empty_evidence(),
            stderr: empty_evidence(),
        };
        for execution in executions
            .into_iter()
            .filter(|execution| !known.contains(&execution.id))
        {
            recovered.execution_ids.push(execution.id);
            recovered.stdout = merge_evidence(recovered.stdout, evidence(&execution.stdout));
            recovered.stderr = merge_evidence(recovered.stderr, evidence(&execution.stderr));
        }
        if !recovered.execution_ids.is_empty() || Instant::now() >= deadline {
            return recovered;
        }
        sleep(Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())))
            .await;
    }
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
            description: "Read a bounded prefix of one relative /workspace file.".to_string(),
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
            description:
                "Overwrite one strictly relative /workspace file with bounded UTF-8 content."
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
            description: "List one strictly relative directory under /workspace.".to_string(),
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
