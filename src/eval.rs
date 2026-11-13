//! The eval suite: tasks made of a seeded workspace, a prompt and a check
//! script; a runner that runs up to `--concurrency` runs at once, journals
//! every run and can replay it;
//! a per-model report; a comparison of two reports; and a self-test that
//! proves each task's check against its reference solution.
//!
//! A task directory is trusted author input, but it is loaded defensively:
//! only regular files and directories, files opened without following a
//! final symlink, bounded sizes, and seed paths held to the tools' path
//! rules. The check script never enters a model request.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::api::{Reconnect, SandboxApi};
use crate::audit::JsonlAudit;
use crate::engine::{
    CheckOutcome, Harness, HarnessConfig, HarnessError, MAX_SEED_BYTES, MAX_SEED_FILES,
    MAX_SESSION_TIMEOUT, MAX_STEPS, MAX_TASK_TEXT_BYTES, MAX_TOOL_CALLS, SeedFile, SessionTask,
    TaskCheck, seed_problem,
};
use crate::files::{InputFileError, read_regular_file};
use crate::journal::{JournalEvent, JsonlJournal, LoadedJournal, read_journal};
use crate::provider::{ModelResponse, Provider, ProviderInfo, ScriptedProvider, ToolCall};
use crate::providers::default_system_prompt;
use crate::replay::ReplayProvider;
use crate::tools::ToolLimits;

/// The only `task.json` schema.
pub const TASK_SCHEMA: u32 = 1;
/// Schema of `manifest.json` and `report.json`.
pub const REPORT_SCHEMA: u32 = 1;
/// Largest `check.sh` or `solution.sh`, before the tools' stdin limit.
pub const MAX_TASK_SCRIPT_BYTES: usize = 64 << 10;
/// Bounds for `check_timeout_secs`.
pub const MIN_CHECK_TIMEOUT_SECS: u64 = 1;
pub const MAX_CHECK_TIMEOUT_SECS: u64 = 600;
/// Bounds for `--trials`.
pub const MAX_TRIALS: u32 = 100;
/// Bounds for `--concurrency`. kilnd's default cap on command streams per
/// user is 8 and Health does not report it; each run holds at most one
/// `ExecStream` at a time, since the model's `exec` is its only streamed
/// call (seeding, file tools and the check are unary).
pub const MAX_CONCURRENCY: u32 = 8;
/// Longest `--label`.
pub const MAX_LABEL_BYTES: usize = 128;
/// Largest `task.json`.
const MAX_TASK_JSON_BYTES: usize = 64 << 10;
/// Most entries, files and directories together, under one task's `seed/`.
const MAX_SEED_ENTRIES: usize = 1024;
/// Most task directories one suite holds.
const MAX_TASKS: usize = 1000;

#[derive(Debug, Error)]
pub enum EvalError {
    /// A task that cannot be loaded, named by its directory.
    #[error("task {task}: {message}")]
    Task { task: String, message: String },
    /// The tasks directory itself, or a `--task` filter.
    #[error("{0}")]
    Tasks(String),
    /// An invalid option.
    #[error("{0}")]
    Options(String),
    /// An output that could not be created or written.
    #[error("{path}: {message}")]
    Output { path: PathBuf, message: String },
    /// kilnd did not answer Health before the suite.
    #[error("kilnd health before the suite failed: {0}")]
    Health(String),
}

fn task_error(task: &str, message: impl Into<String>) -> EvalError {
    EvalError::Task {
        task: task.to_string(),
        message: message.into(),
    }
}

fn output_error(path: &Path, error: impl std::fmt::Display) -> EvalError {
    EvalError::Output {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

/// `task.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskSpec {
    schema: u32,
    id: String,
    /// For people; never model-visible.
    description: String,
    check_timeout_secs: u64,
    session_timeout_secs: Option<u64>,
    max_steps: Option<u32>,
    max_tool_calls: Option<u32>,
}

/// One loaded and validated eval task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalTask {
    pub id: String,
    pub description: String,
    pub prompt: String,
    /// `system.md`, when the task has one; otherwise the CLI's default.
    pub system: Option<String>,
    /// Every regular file under `seed/`, sorted by path.
    pub seed: Vec<SeedFile>,
    pub check: TaskCheck,
    /// The reference solution, a `sh` script; used only by the self-test.
    pub solution: String,
    pub session_timeout: Option<Duration>,
    pub max_steps: Option<u32>,
    pub max_tool_calls: Option<u32>,
}

impl EvalTask {
    /// `base` with this task's overrides.
    #[must_use]
    pub fn config(&self, base: &HarnessConfig) -> HarnessConfig {
        let mut config = base.clone();
        if let Some(timeout) = self.session_timeout {
            config.session_timeout = timeout;
        }
        if let Some(steps) = self.max_steps {
            config.max_steps = steps;
        }
        if let Some(calls) = self.max_tool_calls {
            config.max_tool_calls = calls;
        }
        config
    }

    /// [`EvalTask::config`] with its session timeout capped at `max_run_secs`
    /// seconds (`--max-run-secs`), as one run of the task uses it.
    #[must_use]
    pub fn run_config(&self, base: &HarnessConfig, max_run_secs: Option<u64>) -> HarnessConfig {
        let mut config = self.config(base);
        if let Some(cap) = max_run_secs {
            config.session_timeout = config.session_timeout.min(Duration::from_secs(cap));
        }
        config
    }

    /// The session this task runs under `config`: its prompt, its system
    /// prompt or the default one for `config`'s tool limits, its seed and
    /// its check.
    #[must_use]
    pub fn session(&self, config: &HarnessConfig) -> SessionTask {
        SessionTask {
            system: self
                .system
                .clone()
                .unwrap_or_else(|| default_system_prompt(&config.tool_limits)),
            prompt: self.prompt.clone(),
            seed: self.seed.clone(),
            check: Some(self.check.clone()),
        }
    }
}

/// Whether `id` matches `[a-z0-9][a-z0-9-]{0,63}`.
#[must_use]
pub fn task_id_is_valid(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0] != b'-'
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// Load and validate every task under `dir`, then keep those named in
/// `only` (all of them when it is empty), sorted by ID. Every task loads,
/// filtered or not, so a broken one is never silently skipped.
///
/// # Errors
///
/// Returns the first problem, naming its task, or an unknown `only` ID.
pub fn load_tasks(
    dir: &Path,
    only: &[String],
    base: &HarnessConfig,
) -> Result<Vec<EvalTask>, EvalError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| EvalError::Tasks(format!("tasks directory {}: {error}", dir.display())))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            EvalError::Tasks(format!("tasks directory {}: {error}", dir.display()))
        })?;
        let name = entry.file_name().into_string().map_err(|name| {
            EvalError::Tasks(format!(
                "tasks directory {}: {} is not a UTF-8 name",
                dir.display(),
                name.to_string_lossy()
            ))
        })?;
        let metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|error| task_error(&name, error.to_string()))?;
        if !metadata.file_type().is_dir() {
            return Err(task_error(
                &name,
                "is not a directory (a symlink, file or special file in the tasks directory is \
                 refused)",
            ));
        }
        names.push(name);
        if names.len() > MAX_TASKS {
            return Err(EvalError::Tasks(format!(
                "tasks directory {} holds more than {MAX_TASKS} tasks",
                dir.display()
            )));
        }
    }
    names.sort();
    for id in only {
        if !names.contains(id) {
            return Err(EvalError::Tasks(format!(
                "--task {id:?} names no task in {}",
                dir.display()
            )));
        }
    }
    let mut tasks = Vec::with_capacity(names.len());
    for name in &names {
        tasks.push(load_task(&dir.join(name), name, base)?);
    }
    tasks.retain(|task| only.is_empty() || only.contains(&task.id));
    Ok(tasks)
}

/// The entries a task directory may hold, and whether each is required.
const TASK_ENTRIES: [(&str, bool); 6] = [
    ("task.json", true),
    ("prompt.md", true),
    ("system.md", false),
    ("check.sh", true),
    ("solution.sh", true),
    ("seed", false),
];

/// Load one task directory named `name`.
///
/// # Errors
///
/// Returns a [`EvalError::Task`] naming the task and the problem.
#[allow(clippy::too_many_lines)]
pub fn load_task(dir: &Path, name: &str, base: &HarnessConfig) -> Result<EvalTask, EvalError> {
    let fail = |message: String| task_error(name, message);
    let mut present = BTreeMap::new();
    let entries = std::fs::read_dir(dir).map_err(|error| fail(error.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|error| fail(error.to_string()))?;
        let Ok(entry_name) = entry.file_name().into_string() else {
            return Err(fail(format!(
                "{} is not a UTF-8 name",
                entry.file_name().to_string_lossy()
            )));
        };
        if !TASK_ENTRIES.iter().any(|(known, _)| *known == entry_name) {
            return Err(fail(format!("unknown entry {entry_name:?}")));
        }
        let file_type = std::fs::symlink_metadata(entry.path())
            .map_err(|error| fail(format!("{entry_name}: {error}")))?
            .file_type();
        let expected = if entry_name == "seed" {
            file_type.is_dir()
        } else {
            file_type.is_file()
        };
        if !expected {
            return Err(fail(format!(
                "{entry_name} must be a regular {}, not a symlink or special file",
                if entry_name == "seed" {
                    "directory"
                } else {
                    "file"
                }
            )));
        }
        present.insert(entry_name, entry.path());
    }
    for (entry, required) in TASK_ENTRIES {
        if required && !present.contains_key(entry) {
            return Err(fail(format!("{entry} is missing")));
        }
    }
    let read = |entry: &str, limit: usize| -> Result<Vec<u8>, EvalError> {
        read_regular_file(&present[entry], limit).map_err(|error| match error {
            InputFileError::TooLarge { limit } => {
                fail(format!("{entry} is larger than {limit} bytes"))
            }
            error => fail(format!("{entry}: {error}")),
        })
    };
    let text = |entry: &str, limit: usize| -> Result<String, EvalError> {
        let bytes = read(entry, limit)?;
        if bytes.is_empty() {
            return Err(fail(format!("{entry} is empty")));
        }
        String::from_utf8(bytes).map_err(|_| fail(format!("{entry} is not UTF-8")))
    };

    let spec: TaskSpec = serde_json::from_slice(&read("task.json", MAX_TASK_JSON_BYTES)?)
        .map_err(|error| fail(format!("task.json: {error}")))?;
    if spec.schema != TASK_SCHEMA {
        return Err(fail(format!(
            "task.json schema {} is unsupported; expected {TASK_SCHEMA}",
            spec.schema
        )));
    }
    if !task_id_is_valid(&spec.id) {
        return Err(fail(format!(
            "task.json id {:?} must match [a-z0-9][a-z0-9-]{{0,63}}",
            spec.id
        )));
    }
    if spec.id != name {
        return Err(fail(format!(
            "task.json id {:?} differs from the directory name",
            spec.id
        )));
    }
    let in_range = |value: u64, max: u64| (1..=max).contains(&value);
    if !in_range(spec.check_timeout_secs, MAX_CHECK_TIMEOUT_SECS) {
        return Err(fail(format!(
            "check_timeout_secs must be {MIN_CHECK_TIMEOUT_SECS} to {MAX_CHECK_TIMEOUT_SECS}"
        )));
    }
    if spec
        .session_timeout_secs
        .is_some_and(|secs| !in_range(secs, MAX_SESSION_TIMEOUT.as_secs()))
    {
        return Err(fail(format!(
            "session_timeout_secs must be 1 to {}",
            MAX_SESSION_TIMEOUT.as_secs()
        )));
    }
    if spec
        .max_steps
        .is_some_and(|steps| !in_range(u64::from(steps), u64::from(MAX_STEPS)))
    {
        return Err(fail(format!("max_steps must be 1 to {MAX_STEPS}")));
    }
    if spec
        .max_tool_calls
        .is_some_and(|calls| !in_range(u64::from(calls), u64::from(MAX_TOOL_CALLS)))
    {
        return Err(fail(format!(
            "max_tool_calls must be 1 to {MAX_TOOL_CALLS}"
        )));
    }

    let script_limit = MAX_TASK_SCRIPT_BYTES.min(base.tool_limits.max_stdin_bytes);
    let task = EvalTask {
        id: spec.id.clone(),
        description: spec.description,
        prompt: text("prompt.md", MAX_TASK_TEXT_BYTES)?,
        system: present
            .contains_key("system.md")
            .then(|| text("system.md", MAX_TASK_TEXT_BYTES))
            .transpose()?,
        seed: match present.get("seed") {
            Some(seed) => read_seed(seed, name, base.tool_limits.max_file_bytes)?,
            None => Vec::new(),
        },
        check: TaskCheck {
            script: text("check.sh", script_limit)?.into_bytes(),
            timeout: Duration::from_secs(spec.check_timeout_secs),
        },
        solution: text("solution.sh", script_limit)?,
        session_timeout: spec.session_timeout_secs.map(Duration::from_secs),
        max_steps: spec.max_steps,
        max_tool_calls: spec.max_tool_calls,
    };
    let config = task.config(base);
    if task.check.timeout.saturating_mul(2) > config.session_timeout {
        return Err(fail(format!(
            "check_timeout_secs {} is more than half the session timeout of {} s",
            spec.check_timeout_secs,
            config.session_timeout.as_secs()
        )));
    }
    config
        .validate()
        .map_err(|error| fail(format!("limits: {error}")))?;
    task.session(&config)
        .validate(&config)
        .map_err(|error| fail(error.to_string()))?;
    Ok(task)
}

/// Every regular file under `root`, by path relative to it, sorted.
fn read_seed(root: &Path, task: &str, max_file_bytes: usize) -> Result<Vec<SeedFile>, EvalError> {
    let fail = |message: String| task_error(task, message);
    let mut files: Vec<SeedFile> = Vec::new();
    let mut total = 0u64;
    let mut entries = 0usize;
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((directory, prefix)) = pending.pop() {
        let shown = |name: &str| {
            if prefix.is_empty() {
                format!("seed/{name}")
            } else {
                format!("seed/{prefix}/{name}")
            }
        };
        let listing =
            std::fs::read_dir(&directory).map_err(|error| fail(format!("seed: {error}")))?;
        for entry in listing {
            let entry = entry.map_err(|error| fail(format!("seed: {error}")))?;
            entries += 1;
            if entries > MAX_SEED_ENTRIES {
                return Err(fail(format!(
                    "seed holds more than {MAX_SEED_ENTRIES} entries"
                )));
            }
            let Ok(name) = entry.file_name().into_string() else {
                return Err(fail(format!(
                    "{} is not a UTF-8 name",
                    shown(&entry.file_name().to_string_lossy())
                )));
            };
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let file_type = std::fs::symlink_metadata(entry.path())
                .map_err(|error| fail(format!("{}: {error}", shown(&name))))?
                .file_type();
            if file_type.is_dir() {
                pending.push((entry.path(), relative));
            } else if file_type.is_file() {
                if files.len() >= MAX_SEED_FILES {
                    return Err(fail(format!("seed holds more than {MAX_SEED_FILES} files")));
                }
                let contents = read_regular_file(&entry.path(), max_file_bytes).map_err(
                    |error| match error {
                        InputFileError::TooLarge { limit } => fail(format!(
                            "{} is larger than the {limit}-byte file limit",
                            shown(&name)
                        )),
                        error => fail(format!("{}: {error}", shown(&name))),
                    },
                )?;
                total = total.saturating_add(contents.len() as u64);
                if total > MAX_SEED_BYTES {
                    return Err(fail(format!(
                        "seed holds more than {MAX_SEED_BYTES} bytes in total"
                    )));
                }
                files.push(SeedFile {
                    path: relative,
                    contents,
                });
            } else {
                return Err(fail(format!(
                    "{} is a symlink, FIFO, socket or device; only regular files and \
                     directories are allowed",
                    shown(&name)
                )));
            }
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    if let Some(problem) = seed_problem(
        files
            .iter()
            .map(|file| (file.path.as_str(), file.contents.len())),
        max_file_bytes,
    ) {
        return Err(fail(problem));
    }
    Ok(files)
}

/// One run's line in `results.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResult {
    pub task: String,
    pub trial: u32,
    /// `None` when the session failed before journaling anything.
    pub session_id: Option<String>,
    /// The session outcome (`cleanup_failed` when deletion was not
    /// confirmed), or `not_started` when nothing was journaled.
    pub outcome: String,
    pub work_outcome: String,
    /// `passed`, `failed` or `error`; `None` when the check did not run.
    pub check: Option<String>,
    pub check_exit_code: Option<i32>,
    pub steps: u32,
    pub tool_calls: u32,
    /// All input tokens: uncached, cache reads and cache writes.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Of `input_tokens`, those read from a prompt cache, when any were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Of `input_tokens`, those written to a prompt cache, when any were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    pub wall_ms: u64,
    /// Truncated responses the session discarded and went on from.
    #[serde(default)]
    pub truncation_recoveries: u32,
    /// `matched`, `diverged_turn:<n>`, `diverged_check` or `failed`; `None`
    /// without `--replay` or for a run that did not complete.
    pub replay: Option<String>,
    pub error: Option<String>,
}

impl RunResult {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.check.as_deref() == Some("passed")
    }
}

/// `manifest.json`: what was run, against what, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub harness_version: String,
    pub label: Option<String>,
    /// The provider kind, such as `anthropic` or `openai-compat`.
    pub provider: String,
    pub model: String,
    /// The base URL's host only: never a key, user, path or query.
    pub base_url_host: Option<String>,
    pub tasks: Vec<String>,
    pub trials: u32,
    /// Runs at a time; missing in older manifests, which ran one.
    #[serde(default = "default_concurrency")]
    pub concurrency: u32,
    /// `--max-run-secs`; missing when unset and in older files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_secs: Option<u64>,
    /// The reasoning effort sent (`--effort`); missing for providers
    /// without one and in older files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Thinking clearing (`--clear-thinking`): `"off"`, or `"keep N"` for
    /// the last N assistant turns; missing for providers without it and in
    /// older files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_thinking: Option<String>,
    /// The output token limit per model response (`--max-output-tokens`);
    /// missing in older files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    pub replay: bool,
    pub started_unix_ms: u64,
    pub ended_unix_ms: u64,
    /// The features kilnd listed before the suite.
    pub features: Vec<String>,
    /// Why the suite stopped before every run, if it did.
    pub stopped: Option<String>,
}

/// A mean and a median; `None` when there were no runs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Stat {
    pub mean: Option<f64>,
    pub median: Option<f64>,
}

impl Stat {
    #[must_use]
    pub fn of(values: &[u64]) -> Self {
        if values.is_empty() {
            return Self {
                mean: None,
                median: None,
            };
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let middle = sorted.len() / 2;
        #[allow(clippy::cast_precision_loss)]
        let median = if sorted.len() % 2 == 1 {
            sorted[middle] as f64
        } else {
            f64::midpoint(sorted[middle - 1] as f64, sorted[middle] as f64)
        };
        #[allow(clippy::cast_precision_loss)]
        let mean = values.iter().map(|value| *value as f64).sum::<f64>() / values.len() as f64;
        Self {
            mean: Some(mean),
            median: Some(median),
        }
    }
}

/// One task's results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskReport {
    pub task: String,
    pub runs: u32,
    pub passed: u32,
    /// `passed / runs`; `None` without runs.
    pub pass_rate: Option<f64>,
    /// How many runs ended with each session outcome.
    pub outcomes: BTreeMap<String, u32>,
    pub steps: Stat,
    pub tool_calls: Stat,
    pub input_tokens: Stat,
    pub output_tokens: Stat,
    pub wall_ms: Stat,
    pub replay_matched: u32,
    /// Diverged at a turn or at the check.
    pub replay_diverged: u32,
    pub replay_failed: u32,
}

/// Sums over every run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Totals {
    pub steps: u64,
    pub tool_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub wall_ms: u64,
}

/// Means over every run; `None` without runs.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Means {
    pub steps: Option<f64>,
    pub tool_calls: Option<f64>,
    pub input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    pub wall_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverallReport {
    pub tasks: u32,
    pub runs: u32,
    pub passed: u32,
    /// The mean of the task pass rates, over tasks with at least one run.
    pub macro_pass_rate: Option<f64>,
    /// The unbiased pass@1 estimate from `trials` runs per task: the mean
    /// over tasks of passed / runs, which equals the macro pass rate.
    pub pass_at_1: Option<f64>,
    pub trials: u32,
    pub totals: Totals,
    pub means: Means,
}

/// `report.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    pub label: Option<String>,
    pub provider: String,
    pub model: String,
    pub trials: u32,
    /// Runs at a time; missing in older reports, which ran one.
    #[serde(default = "default_concurrency")]
    pub concurrency: u32,
    /// `--max-run-secs`; missing when unset and in older files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_secs: Option<u64>,
    /// As in [`Manifest::effort`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// As in [`Manifest::clear_thinking`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_thinking: Option<String>,
    /// As in [`Manifest::max_output_tokens`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    pub stopped: Option<String>,
    pub tasks: Vec<TaskReport>,
    pub overall: OverallReport,
}

/// What a manifest or report without `concurrency` ran at.
fn default_concurrency() -> u32 {
    1
}

#[allow(clippy::cast_precision_loss)]
fn ratio(numerator: u32, denominator: u32) -> Option<f64> {
    (denominator > 0).then(|| f64::from(numerator) / f64::from(denominator))
}

#[allow(clippy::cast_precision_loss)]
fn mean_of(total: u64, count: u32) -> Option<f64> {
    (count > 0).then(|| total as f64 / f64::from(count))
}

/// Build the report from a suite's results. Tasks appear in the
/// manifest's order, those without runs included; results for tasks the
/// manifest does not list are ignored.
#[must_use]
pub fn report(results: &[RunResult], manifest: &Manifest) -> Report {
    let mut tasks = Vec::with_capacity(manifest.tasks.len());
    let mut totals = Totals::default();
    let mut runs = 0u32;
    let mut passed = 0u32;
    for id in &manifest.tasks {
        let mine = results
            .iter()
            .filter(|result| &result.task == id)
            .collect::<Vec<_>>();
        let count = u32::try_from(mine.len()).unwrap_or(u32::MAX);
        let task_passed =
            u32::try_from(mine.iter().filter(|result| result.passed()).count()).unwrap_or(0);
        let mut outcomes = BTreeMap::new();
        let mut replay = (0u32, 0u32, 0u32);
        for result in &mine {
            *outcomes.entry(result.outcome.clone()).or_insert(0) += 1;
            match result.replay.as_deref() {
                Some("matched") => replay.0 += 1,
                Some(status) if status.starts_with("diverged") => replay.1 += 1,
                Some(_) => replay.2 += 1,
                None => {}
            }
            totals.steps += u64::from(result.steps);
            totals.tool_calls += u64::from(result.tool_calls);
            totals.input_tokens = totals.input_tokens.saturating_add(result.input_tokens);
            totals.output_tokens = totals.output_tokens.saturating_add(result.output_tokens);
            totals.wall_ms = totals.wall_ms.saturating_add(result.wall_ms);
        }
        let column = |value: fn(&RunResult) -> u64| {
            Stat::of(&mine.iter().map(|result| value(result)).collect::<Vec<_>>())
        };
        runs += count;
        passed += task_passed;
        tasks.push(TaskReport {
            task: id.clone(),
            runs: count,
            passed: task_passed,
            pass_rate: ratio(task_passed, count),
            outcomes,
            steps: column(|result| u64::from(result.steps)),
            tool_calls: column(|result| u64::from(result.tool_calls)),
            input_tokens: column(|result| result.input_tokens),
            output_tokens: column(|result| result.output_tokens),
            wall_ms: column(|result| result.wall_ms),
            replay_matched: replay.0,
            replay_diverged: replay.1,
            replay_failed: replay.2,
        });
    }
    let rates = tasks
        .iter()
        .filter_map(|task| task.pass_rate)
        .collect::<Vec<_>>();
    #[allow(clippy::cast_precision_loss)]
    let macro_pass_rate =
        (!rates.is_empty()).then(|| rates.iter().sum::<f64>() / rates.len() as f64);
    Report {
        schema: REPORT_SCHEMA,
        label: manifest.label.clone(),
        provider: manifest.provider.clone(),
        model: manifest.model.clone(),
        trials: manifest.trials,
        concurrency: manifest.concurrency,
        max_run_secs: manifest.max_run_secs,
        effort: manifest.effort.clone(),
        clear_thinking: manifest.clear_thinking.clone(),
        max_output_tokens: manifest.max_output_tokens,
        stopped: manifest.stopped.clone(),
        overall: OverallReport {
            tasks: u32::try_from(tasks.len()).unwrap_or(u32::MAX),
            runs,
            passed,
            macro_pass_rate,
            pass_at_1: macro_pass_rate,
            trials: manifest.trials,
            totals,
            means: Means {
                steps: mean_of(totals.steps, runs),
                tool_calls: mean_of(totals.tool_calls, runs),
                input_tokens: mean_of(totals.input_tokens, runs),
                output_tokens: mean_of(totals.output_tokens, runs),
                wall_ms: mean_of(totals.wall_ms, runs),
            },
        },
        tasks,
    }
}

fn percent(rate: Option<f64>) -> String {
    rate.map_or_else(|| "n/a".to_string(), |rate| format!("{:.1}%", rate * 100.0))
}

fn number(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |value| format!("{value:.1}"))
}

/// Text from a report made safe for one Markdown table cell or line.
fn cell(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .replace('|', "\\|")
}

/// `report.md`: a header naming the provider, model and label, then one
/// table row per task and an overall row.
#[must_use]
pub fn report_markdown(report: &Report) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "# Eval report\n");
    let _ = writeln!(
        text,
        "- Provider: {}, model: {}",
        cell(&report.provider),
        cell(&report.model)
    );
    if let Some(label) = &report.label {
        let _ = writeln!(text, "- Label: {}", cell(label));
    }
    let _ = writeln!(text, "- Trials per task: {}", report.trials);
    let _ = writeln!(text, "- Runs at a time: {}", report.concurrency);
    if let Some(secs) = report.max_run_secs {
        let _ = writeln!(text, "- Longest run allowed: {secs} s");
    }
    if let Some(effort) = &report.effort {
        let _ = writeln!(text, "- Effort: {}", cell(effort));
    }
    if let Some(clear_thinking) = &report.clear_thinking {
        let _ = writeln!(text, "- Clear thinking: {}", cell(clear_thinking));
    }
    if let Some(tokens) = report.max_output_tokens {
        let _ = writeln!(text, "- Output limit per response: {tokens} tokens");
    }
    if let Some(stopped) = &report.stopped {
        let _ = writeln!(text, "- Stopped early: {}", cell(stopped));
    }
    let _ = writeln!(
        text,
        "\n| Task | Runs | Passed | Pass rate | Outcomes | Steps (mean / median) | Tool calls \
         (mean / median) | Input tokens (mean) | Output tokens (mean) | Wall ms (mean / median) \
         | Replay matched / diverged / failed |"
    );
    let _ = writeln!(
        text,
        "|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|"
    );
    for task in &report.tasks {
        let outcomes = task
            .outcomes
            .iter()
            .map(|(outcome, count)| format!("{} {count}", cell(outcome)))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            text,
            "| {} | {} | {} | {} | {} | {} / {} | {} / {} | {} | {} | {} / {} | {} / {} / {} |",
            cell(&task.task),
            task.runs,
            task.passed,
            percent(task.pass_rate),
            outcomes,
            number(task.steps.mean),
            number(task.steps.median),
            number(task.tool_calls.mean),
            number(task.tool_calls.median),
            number(task.input_tokens.mean),
            number(task.output_tokens.mean),
            number(task.wall_ms.mean),
            number(task.wall_ms.median),
            task.replay_matched,
            task.replay_diverged,
            task.replay_failed,
        );
    }
    let overall = &report.overall;
    let _ = writeln!(
        text,
        "| **Overall** | {} | {} | {} (macro) | | {} | {} | {} | {} | {} | |",
        overall.runs,
        overall.passed,
        percent(overall.macro_pass_rate),
        number(overall.means.steps),
        number(overall.means.tool_calls),
        number(overall.means.input_tokens),
        number(overall.means.output_tokens),
        number(overall.means.wall_ms),
    );
    let _ = writeln!(
        text,
        "\nMacro pass rate (mean of task pass rates): {}. pass@1 from {} trial(s) per task: {}. \
         Totals: {} steps, {} tool calls, {} input and {} output tokens, {} ms.",
        percent(overall.macro_pass_rate),
        overall.trials,
        percent(overall.pass_at_1),
        overall.totals.steps,
        overall.totals.tool_calls,
        overall.totals.input_tokens,
        overall.totals.output_tokens,
        overall.totals.wall_ms,
    );
    text
}

fn delta(base: Option<f64>, new: Option<f64>, scale: f64, unit: &str) -> String {
    match (base, new) {
        (Some(base), Some(new)) => format!("{:+.1}{unit}", (new - base) * scale),
        _ => "n/a".to_string(),
    }
}

fn mean_tokens(task: &TaskReport) -> Option<f64> {
    Some(task.input_tokens.mean? + task.output_tokens.mean?)
}

/// Markdown comparing two reports, such as two models or a harness change:
/// per task (the union, sorted by ID) the pass rate, mean steps and mean
/// total tokens from `base` to `new` with deltas, tasks present on one side
/// only marked, then an overall line, and a note when the two ran a
/// different number of runs at a time.
#[must_use]
pub fn compare(base: &Report, new: &Report) -> String {
    let mut text = String::new();
    let side = |report: &Report| {
        let mut name = format!("{} {}", cell(&report.provider), cell(&report.model));
        if let Some(label) = &report.label {
            let _ = write!(name, " ({})", cell(label));
        }
        let _ = write!(name, ", {} trial(s)", report.trials);
        name
    };
    let _ = writeln!(text, "# Eval comparison\n");
    let _ = writeln!(text, "- Base: {}", side(base));
    let _ = writeln!(text, "- New: {}\n", side(new));
    let _ = writeln!(
        text,
        "| Task | Pass rate (base → new) | Δ | Mean steps (base → new) | Δ | Mean tokens (base → \
         new) | Δ |"
    );
    let _ = writeln!(text, "|---|---|---:|---|---:|---|---:|");
    let ids = base
        .tasks
        .iter()
        .chain(&new.tasks)
        .map(|task| task.task.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for id in ids {
        let left = base.tasks.iter().find(|task| task.task == id);
        let right = new.tasks.iter().find(|task| task.task == id);
        let name = match (left, right) {
            (Some(_), None) => format!("{} (only in base)", cell(id)),
            (None, Some(_)) => format!("{} (only in new)", cell(id)),
            _ => cell(id),
        };
        let rate = |task: Option<&TaskReport>| task.and_then(|task| task.pass_rate);
        let steps = |task: Option<&TaskReport>| task.and_then(|task| task.steps.mean);
        let tokens = |task: Option<&TaskReport>| task.and_then(mean_tokens);
        let _ = writeln!(
            text,
            "| {name} | {} → {} | {} | {} → {} | {} | {} → {} | {} |",
            percent(rate(left)),
            percent(rate(right)),
            delta(rate(left), rate(right), 100.0, " pp"),
            number(steps(left)),
            number(steps(right)),
            delta(steps(left), steps(right), 1.0, ""),
            number(tokens(left)),
            number(tokens(right)),
            delta(tokens(left), tokens(right), 1.0, ""),
        );
    }
    let total_tokens = |report: &Report| {
        Some(report.overall.means.input_tokens? + report.overall.means.output_tokens?)
    };
    let _ = writeln!(
        text,
        "\nOverall macro pass rate: {} → {} ({}); runs {} → {}; mean steps {} → {} ({}); mean \
         tokens {} → {} ({}).",
        percent(base.overall.macro_pass_rate),
        percent(new.overall.macro_pass_rate),
        delta(
            base.overall.macro_pass_rate,
            new.overall.macro_pass_rate,
            100.0,
            " pp"
        ),
        base.overall.runs,
        new.overall.runs,
        number(base.overall.means.steps),
        number(new.overall.means.steps),
        delta(base.overall.means.steps, new.overall.means.steps, 1.0, ""),
        number(total_tokens(base)),
        number(total_tokens(new)),
        delta(total_tokens(base), total_tokens(new), 1.0, ""),
    );
    write_setting_notes(&mut text, base, new);
    text
}

/// The notes [`compare`] ends with: one per run setting the two reports
/// differ in, since those make their numbers not directly comparable.
fn write_setting_notes(text: &mut String, base: &Report, new: &Report) {
    if base.concurrency != new.concurrency {
        let _ = writeln!(
            text,
            "\nRuns at a time differ ({} → {}); wall times are not directly comparable.",
            base.concurrency, new.concurrency
        );
    }
    if base.max_run_secs != new.max_run_secs {
        let secs =
            |secs: Option<u64>| secs.map_or_else(|| "none".to_string(), |secs| secs.to_string());
        let _ = writeln!(
            text,
            "\nLongest run allowed differs ({} → {}); pass rates and wall times are not directly \
             comparable.",
            secs(base.max_run_secs),
            secs(new.max_run_secs)
        );
    }
    // Reports written before these were recorded carry none of them, and an
    // unknown setting is not a known difference.
    if let (Some(base_effort), Some(new_effort)) = (&base.effort, &new.effort)
        && base_effort != new_effort
    {
        let _ = writeln!(
            text,
            "\nEffort differs ({} → {}); pass rates, tokens and wall times are not directly \
             comparable.",
            cell(base_effort),
            cell(new_effort)
        );
    }
    if let (Some(base_clear), Some(new_clear)) = (&base.clear_thinking, &new.clear_thinking)
        && base_clear != new_clear
    {
        let _ = writeln!(
            text,
            "\nThinking clearing differs ({} → {}); pass rates, tokens and wall times are not \
             directly comparable.",
            cell(base_clear),
            cell(new_clear)
        );
    }
    if let (Some(base_tokens), Some(new_tokens)) = (base.max_output_tokens, new.max_output_tokens)
        && base_tokens != new_tokens
    {
        let _ = writeln!(
            text,
            "\nOutput limit per response differs ({base_tokens} → {new_tokens} tokens); pass \
             rates and tokens are not directly comparable."
        );
    }
}

/// Read a `report.json` written by [`run_suite`].
///
/// # Errors
///
/// Returns an error when the file cannot be read or is not a report.
pub fn read_report(dir: &Path) -> Result<Report, EvalError> {
    let path = dir.join("report.json");
    let bytes = read_regular_file(&path, 16 << 20).map_err(|error| output_error(&path, error))?;
    let report: Report =
        serde_json::from_slice(&bytes).map_err(|error| output_error(&path, error))?;
    if report.schema != REPORT_SCHEMA {
        return Err(output_error(
            &path,
            format!(
                "report schema {} is unsupported; expected {REPORT_SCHEMA}",
                report.schema
            ),
        ));
    }
    Ok(report)
}

/// What `eval run` was asked for, besides the tasks.
#[derive(Debug, Clone)]
pub struct SuiteOptions {
    /// Must not exist; created 0700.
    pub out: PathBuf,
    pub trials: u32,
    /// Runs at a time, 1 to [`MAX_CONCURRENCY`].
    pub concurrency: u32,
    /// `--max-run-secs`: each run's session timeout is capped at this many
    /// seconds, and the manifest records it. Launch order still uses each
    /// task's own budget.
    pub max_run_secs: Option<u64>,
    /// The provider's reasoning effort, for the manifest; `None` for a
    /// provider without one.
    pub effort: Option<String>,
    /// The provider's thinking clearing (`"off"` or `"keep N"`), for the
    /// manifest; `None` for a provider without it.
    pub clear_thinking: Option<String>,
    pub replay: bool,
    pub label: Option<String>,
    /// The run limits before each task's overrides.
    pub base_config: HarnessConfig,
    /// The provider kind and model, for the manifest.
    pub provider: ProviderInfo,
    /// The base URL's host only.
    pub base_url_host: Option<String>,
}

impl SuiteOptions {
    /// Check the trial count, concurrency and label.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError::Options`] naming the flag.
    pub fn validate(&self) -> Result<(), EvalError> {
        validate_trials_and_label(self.trials, self.label.as_deref())?;
        validate_concurrency(self.concurrency)
    }
}

/// `--concurrency` must be 1 to [`MAX_CONCURRENCY`].
///
/// # Errors
///
/// Returns [`EvalError::Options`] naming the flag.
pub fn validate_concurrency(concurrency: u32) -> Result<(), EvalError> {
    if (1..=MAX_CONCURRENCY).contains(&concurrency) {
        Ok(())
    } else {
        Err(EvalError::Options(format!(
            "--concurrency must be between 1 and {MAX_CONCURRENCY}"
        )))
    }
}

/// Check `--max-run-secs` against every task: it must be 1 to
/// [`MAX_SESSION_TIMEOUT`] seconds, and each task's capped session timeout
/// (see [`EvalTask::run_config`]) must still be at least twice its check
/// timeout. `None` always passes.
///
/// # Errors
///
/// [`EvalError::Options`] naming the flag, and the task when a check
/// window is the problem.
pub fn check_run_cap(
    tasks: &[EvalTask],
    base: &HarnessConfig,
    cap: Option<u64>,
) -> Result<(), EvalError> {
    let Some(cap) = cap else {
        return Ok(());
    };
    if !(1..=MAX_SESSION_TIMEOUT.as_secs()).contains(&cap) {
        return Err(EvalError::Options(format!(
            "--max-run-secs must be between 1 and {}",
            MAX_SESSION_TIMEOUT.as_secs()
        )));
    }
    for task in tasks {
        let capped = task.run_config(base, Some(cap)).session_timeout;
        if task.check.timeout.saturating_mul(2) > capped {
            return Err(EvalError::Options(format!(
                "--max-run-secs {cap} is less than twice task {}'s check timeout of {} s",
                task.id,
                task.check.timeout.as_secs()
            )));
        }
    }
    Ok(())
}

/// `--trials` must be 1 to [`MAX_TRIALS`], and `--label` at most
/// [`MAX_LABEL_BYTES`] without control characters.
///
/// # Errors
///
/// Returns [`EvalError::Options`] naming the flag.
pub fn validate_trials_and_label(trials: u32, label: Option<&str>) -> Result<(), EvalError> {
    if !(1..=MAX_TRIALS).contains(&trials) {
        return Err(EvalError::Options(format!(
            "--trials must be between 1 and {MAX_TRIALS}"
        )));
    }
    if label
        .is_some_and(|label| label.len() > MAX_LABEL_BYTES || label.chars().any(char::is_control))
    {
        return Err(EvalError::Options(format!(
            "--label must be at most {MAX_LABEL_BYTES} bytes without control characters"
        )));
    }
    Ok(())
}

/// How a suite ended.
#[derive(Debug, Clone, PartialEq)]
pub struct SuiteSummary {
    pub results: Vec<RunResult>,
    pub report: Report,
    /// Why the suite stopped before every run, if it did. `None` means
    /// every run finished with confirmed cleanup.
    pub stopped: Option<String>,
}

/// Refuse an output directory that exists, as anything, before any run.
///
/// # Errors
///
/// Returns [`EvalError::Output`] when `out` exists or cannot be checked.
pub fn refuse_existing_out(out: &Path) -> Result<(), EvalError> {
    match std::fs::symlink_metadata(out) {
        Ok(_) => Err(output_error(out, "--out must not exist")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(output_error(out, error)),
    }
}

fn create_out(out: &Path) -> Result<(), EvalError> {
    refuse_existing_out(out)?;
    crate::outputs::create_new_directory(out).map_err(|error| output_error(out, error))
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), EvalError> {
    let mut file =
        crate::outputs::create_new_file(path).map_err(|error| output_error(path, error))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| output_error(path, error))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Whether a session error means its sandbox may still exist.
fn cleanup_unconfirmed(error: &HarnessError) -> bool {
    matches!(
        error,
        HarnessError::Cleanup(_) | HarnessError::CreateUncertain(_)
    )
}

/// What one session's journal says about it.
#[derive(Debug, Default)]
struct Recorded {
    session_id: Option<String>,
    outcome: Option<(String, String)>,
    steps: u32,
    tool_calls: u32,
    usage: crate::provider::Usage,
    check: Option<CheckOutcome>,
    truncation_recoveries: u32,
}

fn recorded(journal: Option<&LoadedJournal>) -> Recorded {
    let mut recorded = Recorded::default();
    let Some(journal) = journal else {
        return recorded;
    };
    recorded.session_id = journal
        .records
        .first()
        .map(|record| record.session_id.clone());
    // A verified journal requests another turn after a `max_tokens` stop
    // only when the engine discarded it, so recoveries can be counted
    // without `session_ended`; its own count wins when it is there.
    let mut truncated = false;
    for record in &journal.records {
        match &record.event {
            JournalEvent::ModelRequested { turn, .. } => {
                recorded.steps = *turn;
                if std::mem::take(&mut truncated) {
                    recorded.truncation_recoveries += 1;
                }
            }
            JournalEvent::ModelResponded { response, .. } => {
                recorded.usage = recorded.usage + response.usage;
                truncated = response.stop_reason == crate::provider::StopReason::MaxTokens;
            }
            JournalEvent::ToolExecuted { .. } => recorded.tool_calls += 1,
            JournalEvent::CheckCompleted { outcome, .. } => {
                recorded.check = Some(outcome.clone());
            }
            JournalEvent::SessionEnded {
                outcome,
                work_outcome,
                truncation_recoveries,
                ..
            } => {
                recorded.outcome = Some((outcome.clone(), work_outcome.clone()));
                recorded.truncation_recoveries = *truncation_recoveries;
            }
            _ => {}
        }
    }
    recorded
}

/// Run every task `trials` times against providers from `provider`, up to
/// `options.concurrency` runs at once, each in its own sandbox. Runs start
/// longest session timeout first (ties in task order), trials 1 to N, so
/// the runs most likely to be long start before the short ones, and a new
/// one starts as soon as one finishes. Each running run holds its own kilnd
/// connection: kilnd caps every connection's HTTP/2 streams and an
/// `ExecStream` holds one for its whole life, so connections are pooled,
/// reused by later runs, and opened only when none is idle; one run at a
/// time never opens a second.
///
/// Each run writes `OUT/runs/<id>/<trial>/audit.jsonl` and `journal.jsonl`
/// (with `replay`, a replay of that journal against a fresh sandbox writes
/// `replay-audit.jsonl` and `replay-journal.jsonl` beside them) and appends
/// its line to `OUT/results.jsonl` as soon as it ends, so those lines are
/// in completion order; `progress` sees each line in the same order, and
/// an `Err` from it stops the suite like any other output failure, after
/// which it is not called again. [`SuiteSummary::results`], `report.json`
/// and `report.md` are in task and trial order. Result lines, journals and
/// audits are written synchronously on the runtime thread, so a stalled
/// output volume stalls every run in flight; keep `--out` on local storage.
///
/// The suite fails closed. A run whose sandbox deletion was not confirmed
/// (outcome `cleanup_failed`, or a `Cleanup` or `CreateUncertain` error,
/// its replay's included), a run's or replay's outputs that cannot be
/// opened, written or read back, progress output that fails, a provider
/// that cannot be built, a connection that cannot be opened, or
/// cancellation stops it: no new run starts, the runs in flight
/// are canceled, and the suite waits for each of them to finish its own
/// cleanup and writes their lines. No run in flight is ever dropped. Every
/// stop reason is kept, in the order seen, in `stopped`; `canceled` only
/// when cancellation was the only one. Unless `results.jsonl` itself could
/// not be written, `manifest.json`, `report.json` and `report.md` are
/// written for the runs that finished.
///
/// # Errors
///
/// Returns an error when an option is invalid, `OUT` exists, kilnd's Health
/// fails before the first run, or an output cannot be written.
#[allow(clippy::too_many_lines)]
pub async fn run_suite<A, P, F>(
    api: &A,
    tasks: &[EvalTask],
    options: &SuiteOptions,
    mut provider: F,
    cancel: &CancellationToken,
    progress: &mut dyn FnMut(&RunResult) -> Result<(), String>,
) -> Result<SuiteSummary, EvalError>
where
    A: Reconnect,
    P: Provider,
    F: FnMut() -> Result<P, String>,
{
    options.validate()?;
    check_run_cap(tasks, &options.base_config, options.max_run_secs)?;
    refuse_existing_out(&options.out)?;
    let window = options.base_config.lifecycle_timeout;
    let features = match tokio::time::timeout(window, api.health()).await {
        Ok(Ok(health)) => health.capabilities.feature_names(),
        Ok(Err(error)) => return Err(EvalError::Health(error.to_string())),
        Err(_) => {
            return Err(EvalError::Health(format!(
                "no answer within {} ms",
                window.as_millis()
            )));
        }
    };
    create_out(&options.out)?;
    let started_unix_ms = unix_ms();
    let results_path = options.out.join("results.jsonl");
    let mut results_file = crate::outputs::create_new_file(&results_path)
        .map_err(|error| output_error(&results_path, error))?;
    // Every stop cancels this token, never the caller's, so the runs in
    // flight stop and clean up while the suite tells the stops apart.
    let runs_cancel = cancel.child_token();
    // Longest budget first, by each task's own session timeout before
    // `--max-run-secs`, so a cap that ties every task still starts the
    // likely-long ones first; the sort is stable, so ties keep task order.
    // `index` stays the task's place in `tasks`, which orders the results.
    let mut order: Vec<usize> = (0..tasks.len()).collect();
    order.sort_by_key(|&index| {
        std::cmp::Reverse(tasks[index].config(&options.base_config).session_timeout)
    });
    let mut queue = order.into_iter().flat_map(|index| {
        let task = &tasks[index];
        (1..=options.trials).map(move |trial| (index, task, trial))
    });
    let mut idle: Vec<A> = vec![api.clone()];
    let mut running = FuturesUnordered::new();
    let mut finished: Vec<(usize, u32, RunResult)> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    let mut write_error: Option<EvalError> = None;
    let mut progress_failed = false;
    let width = usize::try_from(options.concurrency).unwrap_or(1);
    // No `?`, `return` or `break` leaves this loop while a run is in
    // flight: it ends only when `running` is empty.
    loop {
        while running.len() < width && !runs_cancel.is_cancelled() {
            let Some((index, task, trial)) = queue.next() else {
                break;
            };
            // Nothing has run for this trial when this fails.
            let session_provider = match provider() {
                Ok(session_provider) => session_provider,
                Err(message) => {
                    reasons.push(format!(
                        "task {} trial {trial}: provider: {message}",
                        task.id
                    ));
                    runs_cancel.cancel();
                    break;
                }
            };
            let dir = options
                .out
                .join("runs")
                .join(&task.id)
                .join(trial.to_string());
            running.push(scheduled_run(
                api,
                idle.pop(),
                index,
                task,
                trial,
                options,
                session_provider,
                dir,
                &runs_cancel,
            ));
        }
        let Some(done) = running.next().await else {
            break;
        };
        if let Some(connection) = done.connection {
            idle.push(connection);
        }
        let task_id = &tasks[done.index].id;
        match done.outcome {
            Launched::NotStarted(None) => {}
            Launched::NotStarted(Some(why)) => {
                reasons.push(format!("task {task_id} trial {}: {why}", done.trial));
                runs_cancel.cancel();
            }
            Launched::Ran(Err(error)) => {
                reasons.push(format!("task {task_id} trial {}: {error}", done.trial));
                runs_cancel.cancel();
            }
            Launched::Ran(Ok((result, stop))) => {
                // The stop is known, and the runs in flight told, before
                // any output work.
                if let Some(stop) = stop {
                    reasons.push(format!("task {task_id} trial {}: {stop}", done.trial));
                    runs_cancel.cancel();
                }
                if write_error.is_none() {
                    if let Err(error) = append_line(&mut results_file, &results_path, &result) {
                        write_error = Some(error);
                        runs_cancel.cancel();
                    }
                }
                // Even after a failed write, so a later `cleanup_failed`
                // run still reaches the operator; never after progress
                // itself failed.
                if !progress_failed {
                    if let Err(message) = progress(&result) {
                        progress_failed = true;
                        reasons.push(format!("progress output: {message}"));
                        runs_cancel.cancel();
                    }
                }
                finished.push((done.index, done.trial, result));
            }
        }
    }
    // `running` is empty: every run that started has finished its cleanup.
    if let Some(error) = write_error {
        return Err(error);
    }
    finished.sort_by_key(|(index, trial, _)| (*index, *trial));
    let results: Vec<RunResult> = finished.into_iter().map(|(_, _, result)| result).collect();
    let stopped = if !reasons.is_empty() {
        Some(reasons.join("; "))
    } else if cancel.is_cancelled() {
        Some("canceled".to_string())
    } else {
        None
    };
    let manifest = Manifest {
        schema: REPORT_SCHEMA,
        harness_version: env!("CARGO_PKG_VERSION").to_string(),
        label: options.label.clone(),
        provider: options.provider.name.clone(),
        model: options.provider.model.clone(),
        base_url_host: options.base_url_host.clone(),
        tasks: tasks.iter().map(|task| task.id.clone()).collect(),
        trials: options.trials,
        concurrency: options.concurrency,
        max_run_secs: options.max_run_secs,
        effort: options.effort.clone(),
        clear_thinking: options.clear_thinking.clone(),
        max_output_tokens: Some(options.base_config.max_output_tokens_per_request),
        replay: options.replay,
        started_unix_ms,
        ended_unix_ms: unix_ms(),
        features,
        stopped: stopped.clone(),
    };
    let report = report(&results, &manifest);
    write_json(&options.out.join("manifest.json"), &manifest)?;
    write_json(&options.out.join("report.json"), &report)?;
    write_new_file(
        &options.out.join("report.md"),
        report_markdown(&report).as_bytes(),
    )?;
    Ok(SuiteSummary {
        results,
        report,
        stopped,
    })
}

/// What one scheduled run came back with.
struct Finished<A> {
    /// Index of the task in `tasks`, for canonical ordering.
    index: usize,
    trial: u32,
    /// The run's connection, back to the pool; `None` when none was opened.
    connection: Option<A>,
    outcome: Launched,
}

// One per run, moved once from the run's future to the suite: boxing the
// larger variant would buy nothing.
#[allow(clippy::large_enum_variant)]
enum Launched {
    /// No session started: the connection could not be opened (`Some`, why),
    /// or the suite stopped first (`None`).
    NotStarted(Option<String>),
    Ran(Result<(RunResult, Option<String>), EvalError>),
}

/// Open a connection if none was handed over, then run `run_one`.
#[allow(clippy::too_many_arguments)]
async fn scheduled_run<A: Reconnect, P: Provider>(
    api: &A,
    connection: Option<A>,
    index: usize,
    task: &EvalTask,
    trial: u32,
    options: &SuiteOptions,
    provider: P,
    dir: PathBuf,
    cancel: &CancellationToken,
) -> Finished<A> {
    let not_started = |why: Option<String>| Finished {
        index,
        trial,
        connection: None,
        outcome: Launched::NotStarted(why),
    };
    let connection = if let Some(connection) = connection {
        connection
    } else {
        let window = options.base_config.lifecycle_timeout;
        let opened = tokio::select! {
            biased;
            () = cancel.cancelled() => return not_started(None),
            opened = tokio::time::timeout(window, api.reconnect()) => opened,
        };
        match opened {
            Ok(Ok(connection)) => connection,
            Ok(Err(error)) => return not_started(Some(format!("connect: {error}"))),
            Err(_) => {
                return not_started(Some(format!(
                    "connect: no answer within {} ms",
                    window.as_millis()
                )));
            }
        }
    };
    let ran = run_one(&connection, task, trial, options, provider, &dir, cancel).await;
    Finished {
        index,
        trial,
        connection: Some(connection),
        outcome: Launched::Ran(ran),
    }
}

/// Append `result` to `results.jsonl` and make it durable.
fn append_line(file: &mut std::fs::File, path: &Path, result: &RunResult) -> Result<(), EvalError> {
    let mut line = serde_json::to_vec(result).map_err(|error| output_error(path, error))?;
    line.push(b'\n');
    file.write_all(&line)
        .and_then(|()| file.sync_data())
        .map_err(|error| output_error(path, error))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), EvalError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| output_error(path, error))?;
    bytes.push(b'\n');
    write_new_file(path, &bytes)
}

/// Open a run's audit and journal under `dir`.
fn open_outputs(
    dir: &Path,
    audit: &str,
    journal: &str,
) -> Result<(JsonlAudit, JsonlJournal, PathBuf), EvalError> {
    let audit_path = dir.join(audit);
    let journal_path = dir.join(journal);
    let audit = JsonlAudit::open(&audit_path).map_err(|error| output_error(&audit_path, error))?;
    let journal = match JsonlJournal::open(&journal_path) {
        Ok(journal) => journal,
        Err(error) => {
            audit.discard();
            return Err(output_error(&journal_path, error));
        }
    };
    Ok((audit, journal, journal_path))
}

/// One run, and its replay when asked for. Returns its result line and,
/// when the suite must stop after it, why: a sandbox whose deletion was not
/// confirmed, or run or replay outputs that could not be written, opened or
/// read back. A run that must stop the suite is not replayed.
async fn run_one<A, P>(
    api: &A,
    task: &EvalTask,
    trial: u32,
    options: &SuiteOptions,
    provider: P,
    dir: &Path,
    cancel: &CancellationToken,
) -> Result<(RunResult, Option<String>), EvalError>
where
    A: SandboxApi + Clone,
    P: Provider,
{
    let config = task.run_config(&options.base_config, options.max_run_secs);
    let session = task.session(&config);
    let (audit, journal, journal_path) = open_outputs(dir, "audit.jsonl", "journal.jsonl")?;
    let started = Instant::now();
    let mut harness = Harness::new(api.clone(), provider, audit, journal, config);
    let outcome = harness.run(&session, cancel.clone()).await;
    let wall_ms = elapsed_ms(started);
    drop(harness);
    let read_back = read_journal(&journal_path);
    // A journal known to be empty means the session wrote nothing, which
    // its result already says. Anything else must read back: a journal
    // that is missing, or whose length cannot be read, is a failure too.
    let unreadable = match &read_back {
        Err(error) if !known_empty(&journal_path) => Some(error.to_string()),
        _ => None,
    };
    let loaded = read_back.ok();
    let recorded = recorded(loaded.as_ref());
    let (outcome_name, work_outcome) = recorded.outcome.clone().unwrap_or_else(|| {
        let outcome = match &outcome {
            Err(error) if cleanup_unconfirmed(error) => "cleanup_failed",
            _ if recorded.session_id.is_none() => "not_started",
            _ => "failed",
        };
        (
            outcome.to_string(),
            outcome.replace("cleanup_failed", "failed"),
        )
    });
    let unconfirmed =
        outcome_name == "cleanup_failed" || outcome.as_ref().is_err_and(cleanup_unconfirmed);
    let mut stop = run_stop(unconfirmed, &outcome, unreadable.as_deref());
    let check = outcome
        .as_ref()
        .ok()
        .and_then(|outcome| outcome.check.clone())
        .or(recorded.check);
    let usage = recorded.usage;
    let mut result = RunResult {
        task: task.id.clone(),
        trial,
        session_id: recorded.session_id,
        outcome: outcome_name,
        work_outcome,
        check: check.as_ref().map(|check| check.label().to_string()),
        check_exit_code: check.as_ref().and_then(CheckOutcome::exit_code),
        steps: recorded.steps,
        tool_calls: recorded.tool_calls,
        input_tokens: usage.total_input_tokens(),
        output_tokens: usage.output_tokens,
        cache_read_tokens: (usage.cache_read_input_tokens > 0)
            .then_some(usage.cache_read_input_tokens),
        cache_write_tokens: (usage.cache_creation_input_tokens > 0)
            .then_some(usage.cache_creation_input_tokens),
        wall_ms,
        truncation_recoveries: recorded.truncation_recoveries,
        replay: None,
        error: outcome.as_ref().err().map(ToString::to_string),
    };
    let completed = result.outcome == "completed";
    if replay_due(options.replay, completed, stop.as_deref(), cancel) {
        if let Some(loaded) = &loaded {
            // Replay outputs that cannot be opened or written stop the
            // suite, after this run's line is written.
            match replay_run(api, loaded, &options.base_config, dir, cancel).await {
                Ok(replayed) => {
                    let mut stops = Vec::new();
                    if replayed.unconfirmed {
                        stops.push("the replay's sandbox deletion was not confirmed".to_string());
                    }
                    if let Some(error) = replayed.output_error {
                        stops.push(format!("replay outputs: {error}"));
                        result.replay = Some("failed".to_string());
                    } else {
                        result.replay = Some(replayed.status);
                    }
                    stop = (!stops.is_empty()).then(|| stops.join("; "));
                }
                Err(error) => {
                    result.replay = Some("failed".to_string());
                    stop = Some(format!("replay outputs: {error}"));
                }
            }
        } else {
            result.replay = Some("failed".to_string());
        }
    }
    Ok((result, stop))
}

/// Whether a run is replayed: asked for, completed, and with no reason to
/// stop the suite, which is decided before this, and not canceled.
fn replay_due(
    replay: bool,
    completed: bool,
    stop: Option<&str>,
    cancel: &CancellationToken,
) -> bool {
    replay && completed && stop.is_none() && !cancel.is_cancelled()
}

/// Why the suite must stop after a run, if it must: its sandbox deletion
/// was not confirmed, its audit or journal could not be written, or its
/// journal, which holds something, could not be read back.
fn run_stop<T>(
    unconfirmed: bool,
    outcome: &Result<T, HarnessError>,
    unreadable: Option<&str>,
) -> Option<String> {
    let mut stops = Vec::new();
    if unconfirmed {
        stops.push("sandbox deletion was not confirmed".to_string());
    }
    if let Some(error) = sink_error(outcome) {
        stops.push(format!("run outputs: {error}"));
    }
    if let Some(error) = unreadable {
        stops.push(format!("run journal could not be read back: {error}"));
    }
    (!stops.is_empty()).then(|| stops.join("; "))
}

/// The session's audit or journal error, if that is how it failed.
fn sink_error<T>(outcome: &Result<T, HarnessError>) -> Option<String> {
    match outcome {
        Err(error @ (HarnessError::Audit(_) | HarnessError::Journal(_))) => Some(error.to_string()),
        _ => None,
    }
}

/// Whether `path` is a file whose length reads as 0. A length that cannot
/// be read is never taken for empty.
fn known_empty(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() == 0)
}

/// How a replay went.
struct Replayed {
    /// `matched`, `diverged_turn:<n>`, `diverged_check` or `failed`.
    status: String,
    /// The replay left a sandbox's deletion unconfirmed.
    unconfirmed: bool,
    /// The replay's audit or journal could not be written.
    output_error: Option<String>,
}

/// Replay a recorded run against a fresh sandbox and say how it went.
async fn replay_run<A: SandboxApi + Clone>(
    api: &A,
    recorded: &LoadedJournal,
    base: &HarnessConfig,
    dir: &Path,
    cancel: &CancellationToken,
) -> Result<Replayed, EvalError> {
    let Ok(provider) = ReplayProvider::from_journal(recorded) else {
        return Ok(Replayed {
            status: "failed".to_string(),
            unconfirmed: false,
            output_error: None,
        });
    };
    let expected = provider.recorded_final_answer().map(str::to_string);
    let config = provider.replay_config(base.clone());
    let (audit, journal, _) = open_outputs(dir, "replay-audit.jsonl", "replay-journal.jsonl")?;
    let mut harness = Harness::new(api.clone(), provider, audit, journal, config);
    let result = harness.replay(cancel.clone()).await;
    let replay = harness.provider();
    let status = if let Some(turn) = replay.diverged_at() {
        format!("diverged_turn:{turn}")
    } else {
        match &result {
            Ok(outcome)
                if Some(&outcome.final_answer) == expected.as_ref() && replay.exhausted() =>
            {
                if replay.check_diverged() {
                    "diverged_check".to_string()
                } else {
                    "matched".to_string()
                }
            }
            _ => "failed".to_string(),
        }
    };
    Ok(Replayed {
        status,
        unconfirmed: result.as_ref().is_err_and(cleanup_unconfirmed),
        output_error: sink_error(&result),
    })
}

/// The limits the self-test runs under before each task's overrides: the
/// `run` command's defaults.
#[must_use]
pub fn selftest_config() -> HarnessConfig {
    HarnessConfig {
        max_steps: 40,
        max_tool_calls: 120,
        session_timeout: Duration::from_secs(900),
        max_output_tokens_per_request: 16_384,
        max_total_output_tokens: 600_000,
        tool_limits: ToolLimits {
            timeout: Duration::from_secs(30),
            ..ToolLimits::default()
        },
        ..HarnessConfig::default()
    }
}

/// One task's self-test result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelftestResult {
    pub task: String,
    /// `None` when both sessions behaved: the bare seed failed its check
    /// and the reference solution passed it.
    pub problem: Option<String>,
}

/// How a self-test ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelftestSummary {
    pub results: Vec<SelftestResult>,
    /// Why it stopped before every task, if it did.
    pub stopped: Option<String>,
}

impl SelftestSummary {
    /// Every task was tested and passed.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.stopped.is_none() && self.results.iter().all(|result| result.problem.is_none())
    }
}

/// The scripted model of the self-test's bare session: it answers at once.
#[must_use]
pub fn bare_provider() -> ScriptedProvider {
    ScriptedProvider::new([ModelResponse::final_text("done")])
}

/// The scripted model of the self-test's solved session: one `exec` of
/// `/bin/sh -s` with the reference solution on stdin, then the answer. The
/// solution is this in-process model's own call, so it appears in the
/// session's request history; the self-test never builds a network
/// provider, so it is never sent to one.
#[must_use]
pub fn solved_provider(solution: &str) -> ScriptedProvider {
    ScriptedProvider::new([
        ModelResponse::tool_use([ToolCall::new(
            "call-1",
            "exec",
            json!({"argv": ["/bin/sh", "-s"], "stdin": solution}),
        )]),
        ModelResponse::final_text("done"),
    ])
}

/// Prove each task's check through the real engine with a scripted model
/// and no network provider: a bare
/// session must fail its check, and a session that runs the reference
/// solution must pass it. Audits and journals go under
/// `OUT/<id>/bare/` and `OUT/<id>/solved/`. It fails closed like
/// [`run_suite`].
///
/// # Errors
///
/// Returns an error when `OUT` exists or an output cannot be written.
pub async fn selftest<A: SandboxApi + Clone>(
    api: &A,
    tasks: &[EvalTask],
    base: &HarnessConfig,
    out: &Path,
    cancel: &CancellationToken,
    progress: &mut dyn FnMut(&SelftestResult),
) -> Result<SelftestSummary, EvalError> {
    create_out(out)?;
    let mut results = Vec::new();
    for task in tasks {
        if cancel.is_cancelled() {
            return Ok(SelftestSummary {
                results,
                stopped: Some("canceled".to_string()),
            });
        }
        let config = task.config(base);
        let session = task.session(&config);
        let mut problem = None;
        for (kind, provider, want_pass) in [
            ("bare", bare_provider(), false),
            ("solved", solved_provider(&task.solution), true),
        ] {
            let dir = out.join(&task.id).join(kind);
            let (audit, journal, _) = open_outputs(&dir, "audit.jsonl", "journal.jsonl")?;
            let mut harness = Harness::new(api.clone(), provider, audit, journal, config.clone());
            let result = harness.run(&session, cancel.clone()).await;
            let found = match result {
                Err(error) if cleanup_unconfirmed(&error) => {
                    results.push(SelftestResult {
                        task: task.id.clone(),
                        problem: Some(format!("{kind}: {error}")),
                    });
                    return Ok(SelftestSummary {
                        results,
                        stopped: Some(format!(
                            "task {} {kind}: sandbox deletion was not confirmed",
                            task.id
                        )),
                    });
                }
                Err(error) => Err(format!("{kind}: session failed: {error}")),
                Ok(outcome) => match (outcome.check, want_pass) {
                    (Some(CheckOutcome::Passed), true)
                    | (Some(CheckOutcome::Failed { .. }), false) => Ok(()),
                    (Some(CheckOutcome::Passed), false) => {
                        Err(format!("{kind}: the check passed on the bare seed"))
                    }
                    (Some(check), _) => Err(format!("{kind}: the check {check}")),
                    (None, _) => Err(format!("{kind}: the check did not run")),
                },
            };
            if let Err(found) = found {
                problem.get_or_insert(found);
            }
        }
        let result = SelftestResult {
            task: task.id.clone(),
            problem,
        };
        progress(&result);
        results.push(result);
    }
    Ok(SelftestSummary {
        results,
        stopped: cancel.is_cancelled().then(|| "canceled".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditError;
    use crate::journal::JournalError;

    fn audit_failed() -> Result<(), HarnessError> {
        Err(HarnessError::Audit(AuditError::Io(std::io::Error::other(
            "disk gone",
        ))))
    }

    #[test]
    fn only_a_journal_read_as_empty_counts_as_empty() {
        let dir = std::env::temp_dir().join(format!(
            "kiln-harness-known-empty-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        std::fs::create_dir(&dir).expect("temporary directory");
        let empty = dir.join("empty.jsonl");
        let written = dir.join("written.jsonl");
        std::fs::write(&empty, b"").expect("empty file");
        std::fs::write(&written, b"{}\n").expect("written file");
        assert!(known_empty(&empty));
        assert!(!known_empty(&written));
        // Missing, or not a file: its length is unknown, never empty.
        assert!(!known_empty(&dir.join("missing.jsonl")));
        assert!(!known_empty(&dir));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn audit_and_journal_errors_stop_the_suite_and_others_do_not() {
        // An audit that fails after the work and the journal succeeded.
        assert_eq!(
            run_stop(false, &audit_failed(), None).as_deref(),
            Some("run outputs: audit I/O failed: disk gone")
        );
        let journal: Result<(), HarnessError> = Err(HarnessError::Journal(JournalError::Io(
            std::io::Error::other("disk gone"),
        )));
        assert_eq!(
            run_stop(false, &journal, None).as_deref(),
            Some("run outputs: journal I/O failed: disk gone")
        );
        // A replay's sink error is classified the same way.
        assert_eq!(
            sink_error(&journal).as_deref(),
            Some("journal I/O failed: disk gone")
        );
        for other in [
            Ok(()),
            Err(HarnessError::Canceled),
            Err(HarnessError::Deadline),
            Err(HarnessError::Seed("no space".to_string())),
        ] {
            assert_eq!(sink_error(&other), None);
            assert_eq!(run_stop(false, &other, None), None);
        }
    }

    #[test]
    fn a_run_that_stops_the_suite_is_not_replayed() {
        let live = CancellationToken::new();
        assert!(replay_due(true, true, None, &live));
        // An audit that failed after a completed session.
        let stop = run_stop(false, &audit_failed(), None);
        assert!(!replay_due(true, true, stop.as_deref(), &live));
        assert!(!replay_due(false, true, None, &live));
        assert!(!replay_due(true, false, None, &live));
        let canceled = CancellationToken::new();
        canceled.cancel();
        assert!(!replay_due(true, true, None, &canceled));
    }

    #[test]
    fn every_stop_reason_is_kept_in_order() {
        assert_eq!(
            run_stop(true, &audit_failed(), Some("journal line 3 is empty")).as_deref(),
            Some(
                "sandbox deletion was not confirmed; run outputs: audit I/O failed: disk gone; \
                 run journal could not be read back: journal line 3 is empty"
            )
        );
        assert_eq!(
            run_stop(true, &Ok(()), None).as_deref(),
            Some("sandbox deletion was not confirmed")
        );
    }
}
