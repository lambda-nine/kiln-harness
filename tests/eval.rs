//! The eval suite: task loading, seeding, the task check, its journal
//! records and replay, the runner, reports, comparisons and the CLI.
mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{DeleteBehavior, FakeApi, ShAnswer, ShHandler, api_error, short_lifecycle_config};
use kiln_harness::api::{
    ApiError, Deleted, Execution, ExecutionState, FinalCheck, Health, Reconnect, Sandbox,
    SandboxApi, SandboxExec, SandboxRequest, StreamObserver, StreamedExec,
};
use kiln_harness::audit::{AuditError, AuditEvent, AuditSink, MemoryAudit};
use kiln_harness::engine::{
    CheckOutcome, Harness, HarnessConfig, HarnessError, SeedFile, SessionTask, TaskCheck,
};
use kiln_harness::eval::{
    self, EvalError, EvalTask, Manifest, Report, RunResult, Stat, SuiteOptions, load_tasks,
};
use kiln_harness::journal::{
    JournalEvent, LoadedJournal, MAX_JOURNAL_LINE_BYTES, MemoryJournal, RecordedSeedFile,
    read_journal,
};
use kiln_harness::provider::{
    ModelRequest, ModelResponse, Provider, ProviderError, ProviderInfo, ScriptedProvider, ToolCall,
};
use kiln_harness::replay::ReplayProvider;
use kiln_harness::tools::{EXEC_STREAM_DESCRIPTION, ToolRegistry};
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const CHECK_MARKER: &str = "CHECK-MARKER-7f3a91";
const SOLUTION_MARKER: &str = "SOLUTION-MARKER-c2e604";

// ---------------------------------------------------------------------------
// Helpers

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create parents");
    std::fs::write(path, bytes).expect("write");
}

fn task_json(id: &str) -> Value {
    json!({
        "schema": 1,
        "id": id,
        "description": "a test task",
        "check_timeout_secs": 10
    })
}

/// Write a task directory under `root` with a check that the fake answers
/// by looking for `/workspace/out.txt`.
fn make_task(root: &Path, id: &str, json: &Value, seed: &[(&str, &[u8])]) -> PathBuf {
    let dir = root.join(id);
    write(&dir.join("task.json"), json.to_string().as_bytes());
    write(&dir.join("prompt.md"), b"Write out.txt.\n");
    write(
        &dir.join("check.sh"),
        format!("cd /workspace || exit 2\n# {CHECK_MARKER}\ntest -f out.txt\n").as_bytes(),
    );
    write(
        &dir.join("solution.sh"),
        format!("# {SOLUTION_MARKER}\necho done > /workspace/out.txt\n").as_bytes(),
    );
    for (path, contents) in seed {
        write(&dir.join("seed").join(path), contents);
    }
    dir
}

fn config() -> HarnessConfig {
    short_lifecycle_config()
}

fn load_one(root: &Path, id: &str) -> Result<EvalTask, EvalError> {
    load_tasks(root, &[id.to_string()], &config()).map(|mut tasks| tasks.remove(0))
}

fn loader_error(root: &Path, id: &str) -> String {
    let error = load_one(root, id).expect_err("the task is refused");
    let text = error.to_string();
    assert!(
        text.contains(&format!("task {id}")),
        "names the task: {text}"
    );
    text
}

/// A fake `/bin/sh`: a script that mentions `out.txt` passes when the file
/// exists; one that echoes into it (a solution) writes it.
fn out_txt_handler() -> ShHandler {
    Arc::new(|script: &[u8], files: &mut BTreeMap<String, Vec<u8>>| {
        let script = String::from_utf8_lossy(script);
        if script.contains("echo done > /workspace/out.txt") {
            files.insert("/workspace/out.txt".to_string(), b"done\n".to_vec());
            return ShAnswer::exit(0);
        }
        ShAnswer::exit(i32::from(!files.contains_key("/workspace/out.txt")))
    })
}

/// A fake `/bin/sh` that gives the same answer every time.
fn fixed_handler(answer: ShAnswer) -> ShHandler {
    Arc::new(move |_: &[u8], _: &mut BTreeMap<String, Vec<u8>>| answer.clone())
}

fn checked_task(script: &str, timeout: Duration) -> SessionTask {
    SessionTask {
        check: Some(TaskCheck {
            script: script.as_bytes().to_vec(),
            timeout,
        }),
        ..SessionTask::new("Use the tools.", "Write out.txt.")
    }
}

fn write_out() -> ModelResponse {
    ModelResponse::tool_use([ToolCall::new(
        "call-1",
        "write_file",
        json!({"path": "out.txt", "content": "done\n"}),
    )])
}

/// What the model was sent, and how many Execs kilnd had seen at each call.
#[derive(Clone, Default)]
struct Seen {
    requests: Arc<Mutex<Vec<String>>>,
    execs_at_call: Arc<Mutex<Vec<usize>>>,
}

/// A scripted model that records every request it is sent.
struct Observing {
    api: FakeApi,
    inner: ScriptedProvider,
    seen: Seen,
    delay: Duration,
}

impl Observing {
    fn new(api: &FakeApi, seen: &Seen, responses: Vec<ModelResponse>) -> Self {
        Self {
            api: api.clone(),
            inner: ScriptedProvider::new(responses),
            seen: seen.clone(),
            delay: Duration::ZERO,
        }
    }
}

#[async_trait]
impl Provider for Observing {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "scripted".to_string(),
            model: "observing".to_string(),
        }
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let everything = json!({
            "system": request.system,
            "tools": request.tools,
            "messages": request.messages,
        });
        self.seen
            .requests
            .lock()
            .expect("lock")
            .push(everything.to_string());
        let execs = self.api.state().execs.len();
        self.seen.execs_at_call.lock().expect("lock").push(execs);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.inner.complete(request).await
    }
}

fn argv(api: &FakeApi) -> Vec<Vec<String>> {
    api.state()
        .execs
        .iter()
        .map(|exec| exec.argv.clone())
        .collect()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

fn ended(journal: &MemoryJournal) -> (String, String, bool) {
    match &journal.records.last().expect("records").event {
        JournalEvent::SessionEnded {
            outcome,
            work_outcome,
            sandbox_deleted,
            ..
        } => (outcome.clone(), work_outcome.clone(), *sandbox_deleted),
        other => panic!("last record is {other:?}"),
    }
}

fn event_names(records: &[kiln_harness::journal::JournalRecord]) -> Vec<String> {
    records
        .iter()
        .map(|record| {
            serde_json::to_value(&record.event).expect("event")["event"]
                .as_str()
                .expect("event name")
                .to_string()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Loader

#[test]
fn a_valid_task_loads_with_its_seed_sorted() {
    let temp = common::private_tempdir().expect("tempdir");
    let root = temp.path();
    let mut json = task_json("good-task");
    json["session_timeout_secs"] = json!(60);
    json["max_steps"] = json!(7);
    json["max_tool_calls"] = json!(9);
    let dir = make_task(
        root,
        "good-task",
        &json,
        &[
            ("b.txt", b"b"),
            ("a/z.txt", b"z"),
            ("a/b/c", b""),
            ("a.txt", b"a"),
        ],
    );
    write(&dir.join("system.md"), b"Be careful.\n");

    let task = load_one(root, "good-task").expect("loads");
    assert_eq!(task.id, "good-task");
    assert_eq!(task.system.as_deref(), Some("Be careful.\n"));
    assert_eq!(
        task.seed
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        ["a.txt", "a/b/c", "a/z.txt", "b.txt"]
    );
    assert_eq!(task.check.timeout, Duration::from_secs(10));
    assert!(task.solution.contains(SOLUTION_MARKER));
    let config = task.config(&config());
    assert_eq!(config.session_timeout, Duration::from_secs(60));
    assert_eq!(config.max_steps, 7);
    assert_eq!(config.max_tool_calls, 9);
    let session = task.session(&config);
    assert_eq!(session.system, "Be careful.\n");
    assert_eq!(session.check, Some(task.check.clone()));
}

#[test]
fn the_starter_suite_loads() {
    let tasks = load_tasks(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("evals/tasks"),
        &[],
        &eval::selftest_config(),
    )
    .expect("the starter suite loads");
    assert_eq!(
        tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        [
            "config-edit",
            "count-errors",
            "csv-sort",
            "dedupe",
            "fix-failing-test",
            "fix-off-by-one",
            "hello-file",
            "rename-ext",
        ]
    );
    for task in &tasks {
        let script = String::from_utf8(task.check.script.clone()).expect("UTF-8");
        assert!(
            script.starts_with("cd /workspace || exit 2\n"),
            "{}",
            task.id
        );
    }
}

/// A task's seed copied to a host directory, with `/workspace` in its check
/// and solution rewritten to that directory, so both scripts can run under
/// the host's `/bin/sh` as they would in the sandbox. `suite` is the suite's
/// directory relative to the crate, such as `evals/tasks` or `evals/hard`.
struct HostTask {
    _dir: tempfile::TempDir,
    workspace: PathBuf,
    check: String,
    solution: String,
}

fn host_task(suite: &str, id: &str) -> HostTask {
    let task = Path::new(env!("CARGO_MANIFEST_DIR")).join(suite).join(id);
    let dir = common::private_tempdir().expect("tempdir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let seed = task.join("seed");
    if seed.exists() {
        for file in walk(&seed) {
            let relative = file.strip_prefix(&seed).expect("under the seed");
            write(
                &workspace.join(relative),
                &std::fs::read(&file).expect("read"),
            );
        }
    }
    let rewrite = |name: &str| {
        std::fs::read_to_string(task.join(name))
            .expect("read")
            .replace("/workspace", workspace.to_str().expect("utf-8"))
    };
    HostTask {
        check: rewrite("check.sh"),
        solution: rewrite("solution.sh"),
        workspace,
        _dir: dir,
    }
}

/// Run a script as the sandbox runs a check: `/bin/sh -s` with the script on
/// stdin, from `/`. True when it exits 0.
fn host_sh(script: &str) -> bool {
    use std::io::Write as _;
    let mut child = Command::new("/bin/sh")
        .arg("-s")
        .current_dir("/")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn /bin/sh");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(script.as_bytes())
        .expect("write the script");
    child.wait().expect("wait").success()
}

#[test]
fn each_starter_check_fails_bare_and_passes_after_its_solution_on_the_host() {
    let mut tested = 0;
    for entry in std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("evals/tasks"))
        .expect("read the suite")
    {
        let id = entry.expect("entry").file_name();
        let id = id.to_str().expect("utf-8");
        let task = host_task("evals/tasks", id);
        assert!(!host_sh(&task.check), "{id}: the bare seed passed");
        assert!(host_sh(&task.solution), "{id}: the solution failed");
        assert!(host_sh(&task.check), "{id}: the solved check failed");
        tested += 1;
    }
    assert_eq!(tested, 8);
}

const HARD_TASKS: [&str; 12] = [
    "build-order",
    "csv-colsum",
    "ignore-match",
    "ini-merge",
    "ledger-triage",
    "log-rollup",
    "orders-report",
    "patch-apply",
    "rename-function",
    "semver-sort",
    "stak-interp",
    "strutil-spec",
];

/// The round-two hard tasks, which need more overlays than the first eight.
const HARD_TASKS_ROUND_TWO: [&str; 4] = [
    "ignore-match",
    "ledger-triage",
    "patch-apply",
    "stak-interp",
];

#[test]
fn the_hard_suite_loads() {
    let tasks = load_tasks(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("evals/hard"),
        &[],
        &eval::selftest_config(),
    )
    .expect("the hard suite loads");
    assert_eq!(
        tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        HARD_TASKS
    );
    for task in &tasks {
        let script = String::from_utf8(task.check.script.clone()).expect("UTF-8");
        assert!(
            script.starts_with("cd /workspace || exit 2\n"),
            "{}",
            task.id
        );
    }
}

#[test]
fn each_hard_check_fails_bare_and_passes_after_its_solution_on_the_host() {
    let mut tested = 0;
    for entry in std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("evals/hard"))
        .expect("read the suite")
    {
        let id = entry.expect("entry").file_name();
        let id = id.to_str().expect("utf-8");
        let task = host_task("evals/hard", id);
        assert!(!host_sh(&task.check), "{id}: the bare seed passed");
        assert!(host_sh(&task.solution), "{id}: the solution failed");
        assert!(host_sh(&task.check), "{id}: the solved check failed");
        tested += 1;
    }
    assert_eq!(tested, 12);
}

/// Each directory under `tests/fixtures/eval-hard/<id>/` is an overlay: its
/// files, relative to `/workspace`, are written over the task's seed before
/// the check runs. A `fail-*` overlay is a plausible wrong answer, wrong in
/// one way, that the check must refuse; a `pass-*` overlay is a correct
/// answer written differently from the reference solution, which it must
/// accept.
#[test]
fn hard_task_overlays_fail_or_pass_as_named() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eval-hard");
    let mut ids = std::fs::read_dir(&fixtures)
        .expect("read the overlays")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf-8")
        })
        .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(ids, HARD_TASKS, "one overlay directory per hard task");
    for id in &ids {
        let mut overlays = std::fs::read_dir(fixtures.join(id))
            .expect("read the task's overlays")
            .map(|entry| entry.expect("entry").path())
            .collect::<Vec<_>>();
        overlays.sort();
        let (mut fails, mut passes) = (0, 0);
        for overlay in &overlays {
            let name = overlay
                .file_name()
                .and_then(|name| name.to_str())
                .expect("utf-8");
            let task = host_task("evals/hard", id);
            for file in walk(overlay) {
                let relative = file.strip_prefix(overlay).expect("under the overlay");
                write(
                    &task.workspace.join(relative),
                    &std::fs::read(&file).expect("read"),
                );
            }
            if name.starts_with("fail-") {
                assert!(!host_sh(&task.check), "{id}/{name} passed the check");
                fails += 1;
            } else if name.starts_with("pass-") {
                assert!(host_sh(&task.check), "{id}/{name} failed the check");
                passes += 1;
            } else {
                panic!("{id}/{name}: an overlay is named fail-* or pass-*");
            }
        }
        let round_two = HARD_TASKS_ROUND_TWO.contains(&id.as_str());
        let min_fails = if round_two { 4 } else { 3 };
        assert!(
            fails >= min_fails,
            "{id}: {fails} fail-* overlays, want at least {min_fails}"
        );
        if round_two
            || matches!(
                id.as_str(),
                "semver-sort" | "csv-colsum" | "build-order" | "log-rollup" | "ini-merge"
            )
        {
            assert!(passes >= 1, "{id}: no pass-* overlay");
        }
    }
}

#[test]
fn fix_off_by_one_refuses_a_lookup_and_an_answer_without_its_loop() {
    // Right for the three values the prompt and the old check named, wrong
    // for every other N.
    let lookup = "#!/bin/sh\ncase \"$1\" in\n  1) echo 1 ;;\n  10) echo 55 ;;\n  100) echo 5050 ;;\n  *) echo 0 ;;\nesac\n";
    // Right for every N, but not the loop the prompt asks to keep.
    let closed_form = "#!/bin/sh\necho $(($1 * ($1 + 1) / 2))\n";
    for (name, script) in [("lookup", lookup), ("closed form", closed_form)] {
        let task = host_task("evals/tasks", "fix-off-by-one");
        write(&task.workspace.join("sum.sh"), script.as_bytes());
        assert!(!host_sh(&task.check), "{name} passed the check");
    }
    let task = host_task("evals/tasks", "fix-off-by-one");
    assert!(!host_sh(&task.check), "the bare seed passed");
    assert!(host_sh(&task.solution), "the solution failed");
    assert!(
        host_sh(&task.check),
        "the reference solution failed the check"
    );
}

#[test]
fn fix_off_by_one_accepts_every_posix_loop_form() {
    // Each is a correct repair that keeps a loop: `until` with the inverted
    // bound, and `while` alone at the end of its line with the condition on
    // the next.
    let until = "#!/bin/sh\nn=$1\ntotal=0\ni=1\nuntil [ \"$i\" -gt \"$n\" ]; do\n  total=$((total + i))\n  i=$((i + 1))\ndone\necho \"$total\"\n";
    let bare_while = "#!/bin/sh\nn=$1\ntotal=0\ni=1\nwhile\n  [ \"$i\" -le \"$n\" ]\ndo\n  total=$((total + i))\n  i=$((i + 1))\ndone\necho \"$total\"\n";
    for (name, script) in [("until", until), ("while alone on its line", bare_while)] {
        let task = host_task("evals/tasks", "fix-off-by-one");
        write(&task.workspace.join("sum.sh"), script.as_bytes());
        assert!(host_sh(&task.check), "the {name} repair failed the check");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn each_loader_rule_refuses_its_task() {
    let temp = common::private_tempdir().expect("tempdir");
    let root = temp.path();

    let dir = make_task(
        &root.join("symlinked-seed-case"),
        "symlinked-seed",
        &task_json("symlinked-seed"),
        &[("a", b"a")],
    );
    std::os::unix::fs::symlink("/etc/hosts", dir.join("seed/link")).expect("symlink");
    assert!(
        loader_error(&root.join("symlinked-seed-case"), "symlinked-seed").contains("seed/link")
    );

    let dir = make_task(
        &root.join("fifo-seed-case"),
        "fifo-seed",
        &task_json("fifo-seed"),
        &[("a", b"a")],
    );
    let made = Command::new("mkfifo")
        .arg(dir.join("seed/pipe"))
        .status()
        .expect("mkfifo");
    assert!(made.success());
    assert!(loader_error(&root.join("fifo-seed-case"), "fifo-seed").contains("seed/pipe"));

    let dir = make_task(
        &root.join("symlinked-check-case"),
        "symlinked-check",
        &task_json("symlinked-check"),
        &[],
    );
    std::fs::remove_file(dir.join("check.sh")).expect("remove");
    std::os::unix::fs::symlink(dir.join("solution.sh"), dir.join("check.sh")).expect("symlink");
    assert!(
        loader_error(&root.join("symlinked-check-case"), "symlinked-check").contains("check.sh")
    );

    // A name that is not a normalized relative path the tools accept.
    make_task(
        &root.join("control-path-case"),
        "control-path",
        &task_json("control-path"),
        &[("bad\nname", b"x")],
    );
    assert!(loader_error(&root.join("control-path-case"), "control-path").contains("normalized"));

    let many = (0..257)
        .map(|index| (format!("f{index:03}"), b"x".as_slice()))
        .collect::<Vec<_>>();
    let many = many
        .iter()
        .map(|(path, contents)| (path.as_str(), *contents))
        .collect::<Vec<_>>();
    make_task(
        &root.join("too-many-case"),
        "too-many",
        &task_json("too-many"),
        &many,
    );
    assert!(loader_error(&root.join("too-many-case"), "too-many").contains("256"));

    let big = vec![0u8; config().tool_limits.max_file_bytes + 1];
    make_task(
        &root.join("too-big-case"),
        "too-big",
        &task_json("too-big"),
        &[("big", &big)],
    );
    assert!(loader_error(&root.join("too-big-case"), "too-big").contains("seed/big"));

    let dir = make_task(
        &root.join("unknown-entry-case"),
        "unknown-entry",
        &task_json("unknown-entry"),
        &[],
    );
    write(&dir.join("notes.txt"), b"x");
    assert!(loader_error(&root.join("unknown-entry-case"), "unknown-entry").contains("notes.txt"));

    make_task(
        &root.join("mismatch-case"),
        "mismatch",
        &task_json("other-id"),
        &[],
    );
    assert!(
        loader_error(&root.join("mismatch-case"), "mismatch")
            .contains("differs from the directory")
    );

    let mut json = task_json("unknown-field");
    json["extra"] = json!(true);
    make_task(
        &root.join("unknown-field-case"),
        "unknown-field",
        &json,
        &[],
    );
    assert!(loader_error(&root.join("unknown-field-case"), "unknown-field").contains("extra"));

    let mut json = task_json("slow-check");
    json["check_timeout_secs"] = json!(16);
    make_task(&root.join("slow-check-case"), "slow-check", &json, &[]);
    assert!(
        loader_error(&root.join("slow-check-case"), "slow-check")
            .contains("half the session timeout")
    );

    let mut json = task_json("slow-override");
    json["session_timeout_secs"] = json!(19);
    make_task(
        &root.join("slow-override-case"),
        "slow-override",
        &json,
        &[],
    );
    assert!(
        loader_error(&root.join("slow-override-case"), "slow-override")
            .contains("half the session timeout")
    );

    let mut json = task_json("Bad_Id");
    json["id"] = json!("Bad_Id");
    make_task(&root.join("Bad_Id-case"), "Bad_Id", &json, &[]);
    assert!(loader_error(&root.join("Bad_Id-case"), "Bad_Id").contains("must match"));

    let mut json = task_json("schema-two");
    json["schema"] = json!(2);
    make_task(&root.join("schema-two-case"), "schema-two", &json, &[]);
    assert!(loader_error(&root.join("schema-two-case"), "schema-two").contains("schema 2"));

    let dir = make_task(
        &root.join("no-prompt-case"),
        "no-prompt",
        &task_json("no-prompt"),
        &[],
    );
    std::fs::remove_file(dir.join("prompt.md")).expect("remove");
    assert!(
        loader_error(&root.join("no-prompt-case"), "no-prompt").contains("prompt.md is missing")
    );

    let dir = make_task(
        &root.join("empty-check-case"),
        "empty-check",
        &task_json("empty-check"),
        &[],
    );
    write(&dir.join("check.sh"), b"");
    assert!(
        loader_error(&root.join("empty-check-case"), "empty-check").contains("check.sh is empty")
    );

    let dir = make_task(
        &root.join("huge-solution-case"),
        "huge-solution",
        &task_json("huge-solution"),
        &[],
    );
    write(&dir.join("solution.sh"), &vec![b'#'; (64 << 10) + 1]);
    assert!(
        loader_error(&root.join("huge-solution-case"), "huge-solution")
            .contains("solution.sh is larger")
    );

    // The whole directory loads only when every task does, filtered or not.
    let mixed = root.join("mixed");
    make_task(&mixed, "fine", &task_json("fine"), &[]);
    make_task(&mixed, "mismatch", &task_json("other-id"), &[]);
    let error = load_tasks(&mixed, &strings(&["fine"]), &config())
        .expect_err("a broken task refuses the suite");
    assert!(error.to_string().contains("task mismatch"), "{error}");
}

#[test]
fn the_task_filter_selects_and_refuses_unknown_ids() {
    let temp = common::private_tempdir().expect("tempdir");
    let root = temp.path();
    for id in ["alpha", "beta", "gamma"] {
        make_task(root, id, &task_json(id), &[]);
    }
    let picked = load_tasks(root, &strings(&["gamma", "alpha"]), &config()).expect("loads");
    assert_eq!(
        picked
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "gamma"]
    );
    let error = load_tasks(root, &strings(&["delta"]), &config()).expect_err("unknown");
    assert!(error.to_string().contains("delta"), "{error}");

    write(&root.join("stray.txt"), b"x");
    let error = load_tasks(root, &[], &config()).expect_err("a stray file");
    assert!(error.to_string().contains("stray.txt"), "{error}");
}

#[tokio::test]
async fn the_engine_refuses_seeds_and_checks_the_loader_would() {
    for (name, task) in [
        (
            "a parent path",
            SessionTask {
                seed: vec![SeedFile {
                    path: "../escape".to_string(),
                    contents: Vec::new(),
                }],
                ..SessionTask::new("s", "p")
            },
        ),
        (
            "unsorted paths",
            SessionTask {
                seed: ["b", "a"]
                    .map(|path| SeedFile {
                        path: path.to_string(),
                        contents: Vec::new(),
                    })
                    .to_vec(),
                ..SessionTask::new("s", "p")
            },
        ),
        (
            "a check over half the session",
            checked_task("exit 0", Duration::from_secs(16)),
        ),
        (
            "a check over the tool ceiling",
            checked_task("exit 0", Duration::from_secs(601)),
        ),
        ("an empty check", checked_task("", Duration::from_secs(1))),
    ] {
        let api = FakeApi::default();
        let mut harness = Harness::new(
            api.clone(),
            ScriptedProvider::new([ModelResponse::final_text("done")]),
            MemoryAudit::default(),
            MemoryJournal::default(),
            config(),
        );
        let error = harness
            .run(&task, CancellationToken::new())
            .await
            .expect_err(name);
        assert!(
            matches!(error, HarnessError::InvalidTask(_)),
            "{name}: {error}"
        );
        assert_eq!(api.state().health_calls, 0, "{name}: nothing was contacted");
        assert!(harness.journal().records.is_empty(), "{name}");
    }
}

// ---------------------------------------------------------------------------
// Seeding

fn seeded_task() -> SessionTask {
    SessionTask {
        seed: vec![
            SeedFile {
                path: "a.txt".to_string(),
                contents: b"one\n".to_vec(),
            },
            SeedFile {
                path: "d/e/f.txt".to_string(),
                contents: b"two".to_vec(),
            },
            SeedFile {
                path: "d/g.txt".to_string(),
                contents: Vec::new(),
            },
        ],
        ..SessionTask::new("Use the tools.", "Look around.")
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn seed_files_are_written_first_in_order_with_the_write_file_argv() {
    let api = FakeApi::default();
    let seen = Seen::default();
    let provider = Observing::new(
        &api,
        &seen,
        vec![
            ModelResponse::tool_use([ToolCall::new("call-1", "workspace_diff", json!({}))]),
            ModelResponse::final_text("done"),
        ],
    );
    let mut harness = Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let task = seeded_task();
    harness
        .run(&task, CancellationToken::new())
        .await
        .expect("completes");

    let state = api.state();
    let execs = state
        .execs
        .iter()
        .map(|exec| (exec.argv.clone(), exec.stdin.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        execs[..5],
        [
            (
                strings(&["/bin/dd", "of=/workspace/a.txt", "status=none"]),
                b"one\n".to_vec()
            ),
            (
                strings(&["/bin/mkdir", "-p", "--", "/workspace/d/e"]),
                Vec::new()
            ),
            (
                strings(&["/bin/dd", "of=/workspace/d/e/f.txt", "status=none"]),
                b"two".to_vec()
            ),
            (
                strings(&["/bin/dd", "of=/workspace/d/g.txt", "status=none"]),
                Vec::new()
            ),
            (
                strings(&[
                    "/usr/bin/find",
                    "/workspace",
                    "-xdev",
                    "-type",
                    "f",
                    "-print0"
                ]),
                Vec::new()
            ),
        ]
    );
    drop(state);
    // Seeding and the baseline ran before the first model call.
    assert_eq!(seen.execs_at_call.lock().expect("lock")[0], 5);

    // The baseline includes the seed, so the diff shows no change.
    let journal = harness.journal();
    let diff = journal
        .records
        .iter()
        .find_map(|record| match &record.event {
            JournalEvent::ToolExecuted { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("diff result");
    assert_eq!(diff, json!({"ok": true, "added": [], "removed": []}));
    let JournalEvent::SessionStarted { seed, check, .. } = &journal.records[0].event else {
        panic!("session start");
    };
    assert_eq!(
        seed,
        &task
            .seed
            .iter()
            .map(RecordedSeedFile::new)
            .collect::<Vec<_>>()
    );
    assert_eq!(check, &None);
    let JournalEvent::SandboxReady {
        baseline_file_count,
        ..
    } = &journal.records[1].event
    else {
        panic!("sandbox ready");
    };
    assert_eq!(*baseline_file_count, 3);

    let audit = &harness.audit().records;
    let seeded = audit
        .iter()
        .position(|record| matches!(record.event, AuditEvent::WorkspaceSeeded { .. }))
        .expect("seeding audited");
    let baseline = audit
        .iter()
        .position(|record| matches!(record.event, AuditEvent::WorkspaceBaseline { .. }))
        .expect("baseline audited");
    assert!(seeded < baseline);
    assert!(matches!(
        &audit[seeded].event,
        AuditEvent::WorkspaceSeeded { files: 3, bytes: 7, execution_ids } if execution_ids.len() == 4
    ));
}

#[tokio::test]
async fn a_task_without_a_seed_writes_no_seed_record() {
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    harness
        .run(&SessionTask::new("s", "p"), CancellationToken::new())
        .await
        .expect("completes");
    assert!(
        !harness
            .audit()
            .records
            .iter()
            .any(|record| matches!(record.event, AuditEvent::WorkspaceSeeded { .. }))
    );
    assert!(!harness.journal().lines[0].contains("\"seed\""));
    assert!(!harness.journal().lines[0].contains("\"check\""));
}

#[tokio::test]
async fn a_failed_seed_write_fails_the_session_before_any_model_call() {
    let api = FakeApi::default();
    api.state().fail_dd_from = Some(2);
    let seen = Seen::default();
    let provider = Observing::new(&api, &seen, vec![ModelResponse::final_text("done")]);
    let mut harness = Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let error = harness
        .run(&seeded_task(), CancellationToken::new())
        .await
        .expect_err("seeding fails");
    let HarnessError::Seed(message) = &error else {
        panic!("not a seed error: {error}");
    };
    assert!(message.contains("d/e/f.txt"), "{message}");
    assert!(
        seen.requests.lock().expect("lock").is_empty(),
        "no model call"
    );
    assert_eq!(
        ended(harness.journal()),
        ("failed".to_string(), "failed".to_string(), true)
    );
    let state = api.state();
    assert_eq!(state.deleted.len(), 1, "cleanup confirmed");
    assert!(
        !state
            .execs
            .iter()
            .any(|exec| exec.argv[0] == "/usr/bin/find"),
        "no baseline after a failed seed"
    );
}

#[tokio::test]
async fn every_execution_runs_in_the_workspace_with_the_fixed_env() {
    // Seeding, the baseline, each tool, the diff's manifest and the check:
    // the same working directory and environment, from constants.
    let api = FakeApi::default();
    api.state().sh_handler = Some(out_txt_handler());
    let task = SessionTask {
        check: Some(TaskCheck {
            script: b"test -f out.txt\n".to_vec(),
            timeout: Duration::from_secs(10),
        }),
        ..seeded_task()
    };
    let calls = [
        ToolCall::new("call-1", "exec", json!({"argv": ["/usr/bin/wc", "a.txt"]})),
        ToolCall::new("call-2", "read_file", json!({"path": "a.txt"})),
        ToolCall::new(
            "call-3",
            "write_file",
            json!({"path": "out.txt", "content": "done\n"}),
        ),
        ToolCall::new("call-4", "list_dir", json!({"path": "."})),
        ToolCall::new("call-5", "workspace_diff", json!({})),
    ];
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new([
            ModelResponse::tool_use(calls),
            ModelResponse::final_text("done"),
        ]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let outcome = harness
        .run(&task, CancellationToken::new())
        .await
        .expect("completes");
    assert_eq!(outcome.check, Some(CheckOutcome::Passed));

    let state = api.state();
    let programs = state
        .execs
        .iter()
        .map(|exec| exec.argv[0].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for program in [
        "/bin/dd",
        "/bin/mkdir",
        "/usr/bin/find",
        "/usr/bin/wc",
        "/bin/stat",
        "/usr/bin/head",
        "/bin/ls",
        "/bin/sh",
    ] {
        assert!(programs.contains(program), "{program} ran: {programs:?}");
    }
    let env = [("TERM", "dumb"), ("NO_COLOR", "1"), ("PAGER", "cat")]
        .map(|(name, value)| kiln_harness::api::EnvVar {
            name: name.to_string(),
            value: value.to_string(),
        })
        .to_vec();
    for exec in &state.execs {
        assert_eq!(exec.cwd, "/workspace", "{:?}", exec.argv);
        assert_eq!(exec.env, env, "{:?}", exec.argv);
    }
}

// ---------------------------------------------------------------------------
// The check

#[tokio::test]
async fn the_check_runs_once_after_the_final_answer_and_final_check() {
    let api = FakeApi::default();
    let scripts = Arc::new(Mutex::new(Vec::new()));
    let recorded = scripts.clone();
    api.state().sh_handler = Some(Arc::new(move |script: &[u8], _: &mut _| {
        recorded.lock().expect("lock").push(script.to_vec());
        ShAnswer {
            stdout: b"checked\n".to_vec(),
            ..ShAnswer::exit(0)
        }
    }));
    let task = checked_task(
        "cd /workspace || exit 2\ntest -f out.txt\n",
        Duration::from_secs(10),
    );
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new([write_out(), ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let outcome = harness
        .run(&task, CancellationToken::new())
        .await
        .expect("completes");

    assert_eq!(outcome.check, Some(CheckOutcome::Passed));
    assert_eq!(
        *scripts.lock().expect("lock"),
        [task.check.as_ref().expect("check").script.clone()]
    );
    let all = argv(&api);
    assert_eq!(all.last(), Some(&strings(&["/bin/sh", "-s"])));
    assert_eq!(
        all.iter().filter(|argv| argv[0] == "/bin/sh").count(),
        1,
        "exactly once"
    );
    assert_eq!(api.state().final_checks, 1);

    let names = event_names(&harness.journal().records);
    assert_eq!(
        names[names.len() - 3..],
        ["model_responded", "check_completed", "session_ended"]
    );
    let JournalEvent::CheckCompleted {
        outcome: recorded,
        execution_id,
        stdout,
        stderr,
    } = &harness.journal().records[names.len() - 2].event
    else {
        panic!("check record");
    };
    assert_eq!(recorded, &CheckOutcome::Passed);
    assert!(execution_id.is_some());
    assert_eq!(stdout, "checked\n");
    assert_eq!(stderr, "");

    let audit = &harness.audit().records;
    let final_check = audit
        .iter()
        .position(|record| matches!(record.event, AuditEvent::FinalCheck { .. }))
        .expect("final check");
    let check = audit
        .iter()
        .position(|record| matches!(record.event, AuditEvent::CheckCompleted { .. }))
        .expect("check audited");
    assert!(final_check < check);
    let AuditEvent::CheckCompleted {
        result,
        exit_code,
        timed_out,
        script,
        execution_ids,
        stdout,
        ..
    } = &audit[check].event
    else {
        panic!("check audit");
    };
    assert_eq!(result, "passed");
    assert_eq!(*exit_code, Some(0));
    assert!(!timed_out);
    assert_eq!(
        script.bytes,
        task.check.as_ref().expect("check").script.len()
    );
    assert_eq!(execution_ids.len(), 1);
    assert_eq!(stdout.observed_bytes, 8);
    let audit_text = serde_json::to_string(audit).expect("audit");
    assert!(!audit_text.contains("checked"), "no output in the audit");
    assert!(!audit_text.contains("out.txt"), "no script in the audit");
}

#[tokio::test]
async fn each_check_result_maps_and_never_changes_the_work_outcome() {
    let cases: Vec<(
        &str,
        Option<ShAnswer>,
        Option<kiln_harness::api::ApiError>,
        CheckOutcome,
    )> = vec![
        (
            "exit 0",
            Some(ShAnswer::exit(0)),
            None,
            CheckOutcome::Passed,
        ),
        (
            "exit 3",
            Some(ShAnswer::exit(3)),
            None,
            CheckOutcome::Failed {
                exit_code: Some(3),
                timed_out: false,
            },
        ),
        (
            "timed out",
            Some(ShAnswer::state(ExecutionState::TimedOut)),
            None,
            CheckOutcome::Failed {
                exit_code: None,
                timed_out: true,
            },
        ),
        (
            "failed to start",
            Some(ShAnswer::state(ExecutionState::Failed)),
            None,
            CheckOutcome::Error {
                reason: "the check execution ended failed".to_string(),
            },
        ),
        (
            "canceled by kilnd",
            Some(ShAnswer::state(ExecutionState::Canceled)),
            None,
            CheckOutcome::Error {
                reason: "the check execution ended canceled".to_string(),
            },
        ),
        (
            "an RPC error",
            None,
            Some(api_error(tonic::Code::Unavailable, "kilnd went away")),
            CheckOutcome::Error {
                reason: String::new(),
            },
        ),
    ];
    for (name, answer, error, expected) in cases {
        let api = FakeApi::default();
        {
            let mut state = api.state();
            state.sh_handler = answer.map(fixed_handler);
            state.sh_error = error;
        }
        let mut harness = Harness::new(
            api.clone(),
            ScriptedProvider::new([ModelResponse::final_text("done")]),
            MemoryAudit::default(),
            MemoryJournal::default(),
            config(),
        );
        let outcome = harness
            .run(
                &checked_task("exit 0", Duration::from_secs(10)),
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let check = outcome.check.expect("a check result");
        if let CheckOutcome::Error { .. } = expected {
            assert!(check.same_verdict(&expected), "{name}: {check:?}");
            if !matches!(&expected, CheckOutcome::Error { reason } if reason.is_empty()) {
                assert_eq!(check, expected, "{name}");
            }
        } else {
            assert_eq!(check, expected, "{name}");
        }
        assert_eq!(
            ended(harness.journal()),
            ("completed".to_string(), "completed".to_string(), true),
            "{name}"
        );
    }
}

#[tokio::test]
async fn check_output_is_bounded_in_the_journal() {
    let api = FakeApi::default();
    api.state().sh_handler = Some(fixed_handler(ShAnswer {
        stdout: vec![b'o'; 10_000],
        stderr: vec![0xff; 5_000],
        ..ShAnswer::exit(1)
    }));
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    harness
        .run(
            &checked_task("exit 1", Duration::from_secs(10)),
            CancellationToken::new(),
        )
        .await
        .expect("completes");
    let (stdout, stderr) = harness
        .journal()
        .records
        .iter()
        .find_map(|record| match &record.event {
            JournalEvent::CheckCompleted { stdout, stderr, .. } => {
                Some((stdout.clone(), stderr.clone()))
            }
            _ => None,
        })
        .expect("check record");
    assert_eq!(stdout.len(), 4096);
    // Decoded first, then cut: 5,000 invalid bytes are replacement
    // characters of three bytes each, cut at a character boundary.
    assert!(stderr.len() <= 4096, "{} bytes", stderr.len());
    assert_eq!(stderr, "\u{fffd}".repeat(1365));
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::CheckCompleted { stdout, .. } if stdout.observed_bytes == 10_000
    )));
}

/// The journal's check output for one stdout stream from the fake.
async fn journaled_check_stdout(stdout: Vec<u8>) -> String {
    let api = FakeApi::default();
    api.state().sh_handler = Some(fixed_handler(ShAnswer {
        stdout,
        ..ShAnswer::exit(0)
    }));
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    harness
        .run(
            &checked_task("exit 0", Duration::from_secs(10)),
            CancellationToken::new(),
        )
        .await
        .expect("completes");
    harness
        .journal()
        .records
        .iter()
        .find_map(|record| match &record.event {
            JournalEvent::CheckCompleted { stdout, .. } => Some(stdout.clone()),
            _ => None,
        })
        .expect("check record")
}

#[tokio::test]
async fn check_output_is_cut_at_a_character_boundary() {
    // A two-byte character straddling the bound is dropped whole.
    let mut straddling = vec![b'a'; 4095];
    straddling.extend("é".repeat(10).as_bytes());
    assert_eq!(journaled_check_stdout(straddling).await, "a".repeat(4095));
    // One that ends exactly at the bound is kept.
    let mut ending = vec![b'a'; 4094];
    ending.extend("é".repeat(10).as_bytes());
    let text = journaled_check_stdout(ending).await;
    assert_eq!(text.len(), 4096);
    assert!(text.ends_with('é'));
    // A four-byte character cut by the source bound is not replaced by a
    // replacement character that would still fit.
    let mut cut = vec![b'a'; 4093];
    cut.extend("😀".as_bytes());
    cut.push(b'z');
    assert_eq!(journaled_check_stdout(cut).await, "a".repeat(4093));
    // Short output, invalid bytes included, is kept whole.
    assert_eq!(
        journaled_check_stdout(b"ok\xffend".to_vec()).await,
        "ok\u{fffd}end"
    );
}

#[tokio::test]
async fn no_check_runs_when_the_session_fails() {
    let api = FakeApi::default();
    api.state().sh_handler = Some(fixed_handler(ShAnswer::exit(0)));
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new([]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let error = harness
        .run(
            &checked_task("exit 0", Duration::from_secs(10)),
            CancellationToken::new(),
        )
        .await
        .expect_err("the script has no answer");
    assert!(matches!(error, HarnessError::Provider(_)), "{error}");
    assert!(!argv(&api).iter().any(|argv| argv[0] == "/bin/sh"));
    assert!(!event_names(&harness.journal().records).contains(&"check_completed".to_string()));
    assert_eq!(ended(harness.journal()).1, "failed");
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_the_check_is_an_error_result() {
    let api = FakeApi::default();
    {
        let mut state = api.state();
        state.sh_handler = Some(fixed_handler(ShAnswer::exit(0)));
        state.sh_delay = Duration::from_secs(8);
    }
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        trigger.cancel();
    });
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let outcome = harness
        .run(&checked_task("exit 0", Duration::from_secs(10)), cancel)
        .await
        .expect("the work completed before the cancellation");
    assert_eq!(
        outcome.check,
        Some(CheckOutcome::Error {
            reason: "canceled".to_string()
        })
    );
    assert!(ended(harness.journal()).2, "cleanup confirmed");
}

/// An audit that cancels the session as the final usability check is
/// audited, after that check's own last look at the token and before the
/// task check starts.
struct CancelAfterFinalCheck {
    inner: MemoryAudit,
    cancel: CancellationToken,
}

impl AuditSink for CancelAfterFinalCheck {
    fn append(
        &mut self,
        session_id: &str,
        sandbox_id: Option<&str>,
        event: AuditEvent,
    ) -> Result<(), AuditError> {
        let final_check = matches!(event, AuditEvent::FinalCheck { .. });
        self.inner.append(session_id, sandbox_id, event)?;
        if final_check {
            self.cancel.cancel();
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_cancellation_set_before_the_check_always_wins() {
    for attempt in 0..128 {
        let api = FakeApi::default();
        api.state().sh_handler = Some(fixed_handler(ShAnswer::exit(0)));
        let cancel = CancellationToken::new();
        let audit = CancelAfterFinalCheck {
            inner: MemoryAudit::default(),
            cancel: cancel.clone(),
        };
        let mut harness = Harness::new(
            api.clone(),
            ScriptedProvider::new([ModelResponse::final_text("done")]),
            audit,
            MemoryJournal::default(),
            short_lifecycle_config(),
        );
        let outcome = harness
            .run(
                &checked_task("exit 0", Duration::from_secs(10)),
                cancel.clone(),
            )
            .await
            .expect("the work completed before the task check");
        assert!(cancel.is_cancelled());
        assert_eq!(
            outcome.check,
            Some(CheckOutcome::Error {
                reason: "canceled".to_string()
            }),
            "attempt {attempt}"
        );
        assert!(
            !argv(&api).iter().any(|argv| argv[0] == "/bin/sh"),
            "attempt {attempt}: the check was sent"
        );
        assert_eq!(
            ended(harness.journal()),
            ("completed".to_string(), "completed".to_string(), true),
            "attempt {attempt}"
        );
    }
}

#[tokio::test]
async fn a_cancellation_by_the_time_the_check_answers_replaces_its_verdict() {
    let api = FakeApi::default();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    // The check answers passed, but the session is canceled while it runs.
    api.state().sh_handler = Some(Arc::new(move |_, _| {
        trigger.cancel();
        ShAnswer {
            stdout: b"checked\n".to_vec(),
            ..ShAnswer::exit(0)
        }
    }));
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let outcome = harness
        .run(&checked_task("exit 0", Duration::from_secs(10)), cancel)
        .await
        .expect("the work completed before the cancellation");
    assert_eq!(
        outcome.check,
        Some(CheckOutcome::Error {
            reason: "canceled".to_string()
        })
    );
    // The execution and its output stay as evidence.
    let (execution_id, stdout) = harness
        .journal()
        .records
        .iter()
        .find_map(|record| match &record.event {
            JournalEvent::CheckCompleted {
                execution_id,
                stdout,
                ..
            } => Some((execution_id.clone(), stdout.clone())),
            _ => None,
        })
        .expect("check record");
    assert!(execution_id.is_some());
    assert_eq!(stdout, "checked\n");
    assert!(harness.audit().records.iter().any(|record| matches!(
        &record.event,
        AuditEvent::CheckCompleted { result, execution_ids, .. }
            if result == "error" && execution_ids.len() == 1
    )));
    assert!(ended(harness.journal()).2, "cleanup confirmed");
}

#[tokio::test(start_paused = true)]
async fn the_session_deadline_during_the_check_is_an_error_result() {
    let api = FakeApi::default();
    {
        let mut state = api.state();
        state.sh_handler = Some(fixed_handler(ShAnswer::exit(0)));
        state.sh_delay = Duration::from_secs(3);
    }
    let config = HarnessConfig {
        session_timeout: Duration::from_secs(4),
        ..config()
    };
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let outcome = harness
        .run(
            &checked_task("exit 0", Duration::from_secs(2)),
            CancellationToken::new(),
        )
        .await
        .expect("the work completed");
    assert_eq!(
        outcome.check,
        Some(CheckOutcome::Error {
            reason: "deadline".to_string()
        })
    );
}

#[tokio::test(start_paused = true)]
async fn the_model_works_until_the_deadline_less_the_check_window() {
    let config = HarnessConfig {
        session_timeout: Duration::from_secs(4),
        ..config()
    };
    let task = checked_task("exit 0", Duration::from_secs(2));

    // A model slower than the work deadline gets the deadline at 2 s, not 4.
    let api = FakeApi::default();
    api.state().sh_handler = Some(fixed_handler(ShAnswer::exit(0)));
    let seen = Seen::default();
    let mut slow = Observing::new(&api, &seen, vec![ModelResponse::final_text("done")]);
    slow.delay = Duration::from_secs(3);
    let mut harness = Harness::new(
        api.clone(),
        slow,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config.clone(),
    );
    let started = Instant::now();
    let error = harness
        .run(&task, CancellationToken::new())
        .await
        .expect_err("the model is too slow");
    assert!(matches!(error, HarnessError::Deadline), "{error}");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < Duration::from_millis(2500),
        "{elapsed:?}"
    );
    assert!(!argv(&api).iter().any(|argv| argv[0] == "/bin/sh"));

    // A model that answers just before the work deadline still leaves the
    // check its whole window.
    let api = FakeApi::default();
    {
        let mut state = api.state();
        state.sh_handler = Some(fixed_handler(ShAnswer::exit(0)));
        state.sh_delay = Duration::from_millis(1500);
    }
    let mut prompt = Observing::new(&api, &seen, vec![ModelResponse::final_text("done")]);
    prompt.delay = Duration::from_millis(1900);
    let mut harness = Harness::new(
        api,
        prompt,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let started = Instant::now();
    let outcome = harness
        .run(&task, CancellationToken::new())
        .await
        .expect("completes");
    assert_eq!(outcome.check, Some(CheckOutcome::Passed));
    assert!(started.elapsed() >= Duration::from_millis(3400));
}

#[tokio::test]
async fn a_check_or_seed_not_run_as_sent_fails_as_an_unusable_sandbox_does() {
    // The check: an error verdict that keeps the execution ID and none of
    // its output; the work outcome is unchanged.
    let api = FakeApi::default();
    api.state().sh_handler = Some(fixed_handler(ShAnswer {
        stdout: b"must not be kept".to_vec(),
        ..ShAnswer::exit(0)
    }));
    // 1 is the baseline, 2 the write, 3 the check.
    api.state().tamper_exec = Some((3, |row| row.env_sha256 = "0".repeat(64)));
    let task = checked_task("test -f out.txt\n", Duration::from_secs(10));
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new([write_out(), ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let outcome = harness
        .run(&task, CancellationToken::new())
        .await
        .expect("the work completed");
    assert_eq!(
        outcome.check,
        Some(CheckOutcome::Error {
            reason: kiln_harness::tools::EXEC_CONTEXT_MISMATCH.to_string()
        })
    );
    let row = api.state().executions[2].id.clone();
    let recorded = harness
        .journal()
        .records
        .iter()
        .find_map(|record| match &record.event {
            JournalEvent::CheckCompleted {
                execution_id,
                stdout,
                ..
            } => Some((execution_id.clone(), stdout.clone())),
            _ => None,
        })
        .expect("check_completed");
    assert_eq!(recorded, (Some(row), String::new()));

    // Seeding: a seed failure, as for an execution that left the sandbox
    // unusable.
    let api = FakeApi::default();
    api.state().tamper_exec = Some((1, |row| row.cwd = "/".to_string()));
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let error = harness
        .run(&seeded_task(), CancellationToken::new())
        .await
        .expect_err("seeding fails");
    assert!(
        matches!(&error, HarnessError::Seed(message)
            if message.contains("no longer usable")
                && message.contains(kiln_harness::tools::EXEC_CONTEXT_MISMATCH)),
        "{error}"
    );
    assert_eq!(api.state().execs.len(), 1, "nothing after it");
}

#[tokio::test]
async fn unusable_refusals_at_seeding_share_the_tools_classification() {
    // M3B-08: a typed refusal of a seed Exec that proves the sandbox
    // unusable, new or existing, is a Seed failure saying so; nothing more
    // is sent, the model is never asked, and cleanup runs.
    for (code, token) in [
        (tonic::Code::Unimplemented, "backend_unsupported:exec_env"),
        (tonic::Code::FailedPrecondition, "sandbox_lost"),
    ] {
        let api = FakeApi::default();
        api.state()
            .features
            .insert(kiln_harness::api::Feature::TypedReasons);
        api.state().tool_exec_error = Some(common::api_error_with_reason(code, token, token));
        let seen = Seen::default();
        let provider = Observing::new(&api, &seen, vec![ModelResponse::final_text("done")]);
        let mut harness = Harness::new(
            api.clone(),
            provider,
            MemoryAudit::default(),
            MemoryJournal::default(),
            config(),
        );
        let error = harness
            .run(&seeded_task(), CancellationToken::new())
            .await
            .expect_err("seeding fails");
        assert!(
            matches!(&error, HarnessError::Seed(message)
                if message.contains("the sandbox is no longer usable") && message.contains(token)),
            "{token}: {error}"
        );
        let state = api.state();
        assert_eq!(
            state.execs.len(),
            1,
            "{token}: no later seed Exec or baseline"
        );
        assert_eq!(state.execs[0].argv[0], "/bin/dd", "{token}");
        assert_eq!(
            state.deleted,
            ["sb-0000000000000001"],
            "{token}: cleaned up"
        );
        drop(state);
        assert!(
            seen.requests.lock().expect("lock").is_empty(),
            "{token}: no model call"
        );
        assert!(
            !harness
                .journal()
                .records
                .iter()
                .any(|record| matches!(record.event, JournalEvent::ModelRequested { .. })),
            "{token}"
        );
        let finished = harness
            .audit()
            .records
            .iter()
            .find_map(|record| match &record.event {
                AuditEvent::SessionFinished {
                    sandbox_deleted, ..
                } => Some(*sandbox_deleted),
                _ => None,
            });
        assert_eq!(finished, Some(true), "{token}");
    }
}

#[tokio::test]
async fn unusable_refusals_at_the_check_share_the_tools_classification() {
    // M3B-04: an unsupported exec option is classified like a lost sandbox.
    for (code, token) in [
        (tonic::Code::Unimplemented, "backend_unsupported:exec_env"),
        (
            tonic::Code::FailedPrecondition,
            "sandbox_unsupported:exec_cwd",
        ),
        (tonic::Code::FailedPrecondition, "sandbox_lost"),
    ] {
        let api = FakeApi::default();
        api.state()
            .features
            .insert(kiln_harness::api::Feature::TypedReasons);
        api.state().sh_error = Some(common::api_error_with_reason(code, token, token));
        let task = checked_task("test -f out.txt\n", Duration::from_secs(10));
        let mut harness = Harness::new(
            api.clone(),
            ScriptedProvider::new([write_out(), ModelResponse::final_text("done")]),
            MemoryAudit::default(),
            MemoryJournal::default(),
            config(),
        );
        let outcome = harness
            .run(&task, CancellationToken::new())
            .await
            .expect("the work completed");
        let Some(CheckOutcome::Error { reason }) = &outcome.check else {
            panic!("{token}: {:?}", outcome.check);
        };
        assert!(
            reason.starts_with("execution left the sandbox unusable: ") && reason.contains(token),
            "{token}: {reason}"
        );
    }

    // Another refusal is an error verdict with its own message.
    let api = FakeApi::default();
    api.state()
        .features
        .insert(kiln_harness::api::Feature::TypedReasons);
    api.state().sh_error = Some(common::api_error_with_reason(
        tonic::Code::Unimplemented,
        "backend_unsupported:sandbox_expiry",
        "backend_unsupported:sandbox_expiry",
    ));
    let task = checked_task("test -f out.txt\n", Duration::from_secs(10));
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([write_out(), ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let outcome = harness
        .run(&task, CancellationToken::new())
        .await
        .expect("the work completed");
    let Some(CheckOutcome::Error { reason }) = &outcome.check else {
        panic!("{:?}", outcome.check);
    };
    assert!(!reason.contains("unusable"), "{reason}");
}

// ---------------------------------------------------------------------------
// Replay

async fn record_checked(api: &FakeApi) -> LoadedJournal {
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("journal.jsonl");
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new([write_out(), ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        kiln_harness::journal::JsonlJournal::open(&path).expect("journal"),
        config(),
    );
    let task = SessionTask {
        seed: vec![SeedFile {
            path: "in.txt".to_string(),
            contents: b"input\n".to_vec(),
        }],
        ..checked_task("test -f /workspace/out.txt", Duration::from_secs(10))
    };
    harness
        .run(&task, CancellationToken::new())
        .await
        .expect("records");
    read_journal(&path).expect("verifies")
}

#[tokio::test]
async fn replay_rebuilds_the_seed_and_check_and_compares_the_check() {
    let api = FakeApi::default();
    api.state().sh_handler = Some(out_txt_handler());
    let recorded = record_checked(&api).await;
    let JournalEvent::SessionStarted { seed, check, .. } = &recorded.records[0].event else {
        panic!("session start");
    };
    assert_eq!(seed.len(), 1);
    assert_eq!(
        check.as_ref().map(|check| check.timeout_ms),
        Some(10_000),
        "{check:?}"
    );

    // The same verdict in a fresh sandbox matches.
    let provider = ReplayProvider::from_journal(&recorded).expect("replayable");
    assert_eq!(provider.recorded_check(), Some(&CheckOutcome::Passed));
    let task = provider.task();
    assert_eq!(task.seed[0].contents, b"input\n");
    assert_eq!(
        task.check.as_ref().map(|check| check.script.clone()),
        Some(b"test -f /workspace/out.txt".to_vec())
    );
    let fresh = FakeApi::default();
    fresh.state().sh_handler = Some(out_txt_handler());
    let mut replay = Harness::new(
        fresh.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let outcome = replay
        .run(&task, CancellationToken::new())
        .await
        .expect("replays");
    assert_eq!(outcome.check, Some(CheckOutcome::Passed));
    assert!(replay.provider().exhausted());
    assert!(!replay.provider().check_diverged());
    assert_eq!(fresh.state().files["/workspace/in.txt"], b"input\n");

    // A sandbox whose check answers differently diverges at the check.
    let provider = ReplayProvider::from_journal(&recorded).expect("replayable");
    let other = FakeApi::default();
    other.state().sh_handler = Some(fixed_handler(ShAnswer::exit(1)));
    let mut replay = Harness::new(
        other,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let outcome = replay
        .run(&task, CancellationToken::new())
        .await
        .expect("replays");
    assert!(matches!(outcome.check, Some(CheckOutcome::Failed { .. })));
    assert_eq!(replay.provider().diverged_at(), None);
    assert!(replay.provider().check_diverged());
}

#[test]
fn any_check_error_matches_any_other() {
    let deadline = CheckOutcome::Error {
        reason: "deadline".to_string(),
    };
    let rpc = CheckOutcome::Error {
        reason: "kilnd went away".to_string(),
    };
    assert!(deadline.same_verdict(&rpc));
    assert!(!deadline.same_verdict(&CheckOutcome::Passed));
    let one = CheckOutcome::Failed {
        exit_code: Some(1),
        timed_out: false,
    };
    let two = CheckOutcome::Failed {
        exit_code: Some(2),
        timed_out: false,
    };
    assert!(!one.same_verdict(&two));
}

#[test]
fn a_journal_without_seed_or_check_is_byte_identical_to_before() {
    // Recorded by the harness before seeds and checks existed.
    let golden =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/journal-demo-golden.jsonl");
    let loaded = read_journal(&golden).expect("the old journal still verifies");
    assert!(loaded.complete);
    ReplayProvider::from_journal(&loaded).expect("and still replays");
    let lines = common::chained_lines(
        "hs-golden",
        loaded
            .records
            .iter()
            .map(|record| record.event.clone())
            .collect(),
    );
    let mut text = lines.join("\n");
    text.push('\n');
    assert_eq!(text, std::fs::read_to_string(&golden).expect("golden"));
}

#[tokio::test]
async fn todays_demo_journal_matches_the_golden_one() {
    // The golden journal predates truncation recovery: with it off, and no
    // recovery, the journal is byte-identical to before but for the
    // execution context and the streaming `exec` description, with the
    // request hashes that include it.
    let mut harness = Harness::new(
        FakeApi::default(),
        common::Unstreamed::new(ScriptedProvider::demo()),
        MemoryAudit::default(),
        MemoryJournal::default(),
        HarnessConfig {
            max_truncation_recoveries: 0,
            ..HarnessConfig::default()
        },
    );
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("demo");
    let registry = ToolRegistry::new(HarnessConfig::default().tool_limits);
    let JournalEvent::SessionStarted { tools, .. } = &harness.journal().records[0].event else {
        panic!("session start");
    };
    // Only the streaming `exec` description differs: it states the cutoff.
    assert_eq!(
        tools[0].description,
        format!(
            "{}{EXEC_STREAM_DESCRIPTION}",
            registry.definitions()[0].description
        )
    );
    assert_eq!(tools[1..], registry.definitions()[1..]);
    // The golden journal also predates the execution context, today always
    // recorded; recorded without it, and without streaming, the rest is
    // unchanged.
    let events = common::recorded_in(&harness.journal().records, None, &harness.provider().hashes)
        .into_iter()
        .map(|record| {
            let mut event = record.event;
            if let JournalEvent::ModelResponded { latency_ms, .. } = &mut event {
                *latency_ms = 0;
            }
            event
        })
        .collect();
    let mut text = common::chained_lines("hs-golden", events).join("\n");
    text.push('\n');
    let golden =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/journal-demo-golden.jsonl");
    assert_eq!(text, std::fs::read_to_string(golden).expect("golden"));
}

// ---------------------------------------------------------------------------
// The runner

fn options(out: &Path, trials: u32, replay: bool) -> SuiteOptions {
    SuiteOptions {
        out: out.to_path_buf(),
        trials,
        concurrency: 1,
        max_run_secs: None,
        effort: None,
        replay,
        label: Some("test | run".to_string()),
        base_config: config(),
        provider: ProviderInfo {
            name: "scripted".to_string(),
            model: "observing".to_string(),
        },
        base_url_host: Some("models.example.com".to_string()),
    }
}

fn two_tasks(root: &Path) -> Vec<EvalTask> {
    for id in ["alpha", "beta"] {
        make_task(root, id, &task_json(id), &[("in.txt", b"input\n")]);
    }
    load_tasks(root, &[], &config()).expect("loads")
}

fn read_lines(path: &Path) -> Vec<RunResult> {
    std::fs::read_to_string(path)
        .expect("results")
        .lines()
        .map(|line| serde_json::from_str(line).expect("result line"))
        .collect()
}

#[tokio::test]
async fn the_runner_writes_results_reports_and_a_manifest() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = two_tasks(&temp.path().join("tasks"));
    let out = temp.path().join("out");
    let api = FakeApi::default();
    api.state().sh_handler = Some(out_txt_handler());
    let seen = Seen::default();
    let mut runs = 0;
    let mut progress = Vec::new();
    // alpha's two runs write out.txt and pass; beta's answer at once and fail.
    let summary = eval::run_suite(
        &api,
        &tasks,
        &options(&out, 2, false),
        || {
            runs += 1;
            let responses = if runs <= 2 {
                vec![write_out(), ModelResponse::final_text("done")]
            } else {
                vec![ModelResponse::final_text("done")]
            };
            Ok(Observing::new(&api, &seen, responses))
        },
        &CancellationToken::new(),
        &mut |result| {
            progress.push(result.clone());
            Ok(())
        },
    )
    .await
    .expect("the suite runs");

    assert_eq!(summary.stopped, None);
    let results = read_lines(&out.join("results.jsonl"));
    assert_eq!(results, summary.results);
    assert_eq!(results, progress);
    assert_eq!(
        results
            .iter()
            .map(|result| (result.task.as_str(), result.trial, result.check.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("alpha", 1, Some("passed")),
            ("alpha", 2, Some("passed")),
            ("beta", 1, Some("failed")),
            ("beta", 2, Some("failed")),
        ]
    );
    for result in &results {
        assert_eq!(result.outcome, "completed");
        assert_eq!(result.work_outcome, "completed");
        assert!(result.session_id.is_some());
        assert_eq!(result.replay, None);
        assert_eq!(result.error, None);
        let run = out
            .join("runs")
            .join(&result.task)
            .join(result.trial.to_string());
        assert!(run.join("audit.jsonl").is_file());
        read_journal(&run.join("journal.jsonl")).expect("each journal verifies");
    }
    assert_eq!(results[0].steps, 2);
    assert_eq!(results[0].tool_calls, 1);
    assert_eq!(results[2].check_exit_code, Some(1));

    let report: Report =
        serde_json::from_slice(&std::fs::read(out.join("report.json")).expect("report"))
            .expect("report JSON");
    assert_eq!(report, summary.report);
    assert_eq!(report.overall.runs, 4);
    assert_eq!(report.overall.macro_pass_rate, Some(0.5));
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).expect("manifest"))
            .expect("manifest JSON");
    assert_eq!(manifest.tasks, ["alpha", "beta"]);
    assert_eq!(manifest.trials, 2);
    assert_eq!(manifest.label.as_deref(), Some("test | run"));
    assert_eq!(
        manifest.base_url_host.as_deref(),
        Some("models.example.com")
    );
    assert_eq!(manifest.stopped, None);
    assert_eq!(manifest.concurrency, 1);
    assert_eq!(
        api.state().reconnects,
        0,
        "one run at a time needs one connection"
    );
    let markdown = std::fs::read_to_string(out.join("report.md")).expect("markdown");
    assert!(
        markdown.contains("| alpha | 2 | 2 | 100.0% |"),
        "{markdown}"
    );
    assert!(markdown.contains("- Runs at a time: 1"), "{markdown}");
    assert!(markdown.contains("| beta | 2 | 0 | 0.0% |"), "{markdown}");
    assert!(markdown.contains("Label: test \\| run"), "{markdown}");

    let mode = std::fs::metadata(&out).expect("out").permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[tokio::test]
async fn neither_script_reaches_the_provider_in_a_suite_run() {
    // `eval run` never reads the solution into a session, and the check
    // never enters a request. (The self-test is different: its scripted
    // in-process model sends the solution to `exec`, but no network
    // provider exists there.)
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = two_tasks(&temp.path().join("tasks"));
    let out = temp.path().join("out");
    let api = FakeApi::default();
    api.state().sh_handler = Some(out_txt_handler());
    let seen = Seen::default();
    eval::run_suite(
        &api,
        &tasks,
        &options(&out, 1, true),
        || {
            Ok(Observing::new(
                &api,
                &seen,
                vec![
                    ModelResponse::tool_use([
                        ToolCall::new("call-1", "list_dir", json!({"path": "."})),
                        ToolCall::new("call-2", "workspace_diff", json!({})),
                        ToolCall::new("call-3", "read_file", json!({"path": "in.txt"})),
                    ]),
                    write_out(),
                    ModelResponse::final_text("done"),
                ],
            ))
        },
        &CancellationToken::new(),
        &mut |_| Ok(()),
    )
    .await
    .expect("the suite runs");
    let requests = seen.requests.lock().expect("lock");
    assert_eq!(requests.len(), 6);
    assert!(requests.iter().any(|request| request.contains("input\\n")));
    for request in requests.iter() {
        assert!(!request.contains(CHECK_MARKER), "{request}");
        assert!(!request.contains(SOLUTION_MARKER), "{request}");
    }
    for entry in walk(&out) {
        if entry
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains("audit") || name == "results.jsonl")
        {
            let text = std::fs::read_to_string(&entry).expect("read");
            assert!(!text.contains(CHECK_MARKER), "{}", entry.display());
        }
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

#[tokio::test]
async fn results_carry_truncation_recoveries_and_replay_them() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = two_tasks(&temp.path().join("tasks"));
    let api = FakeApi::default();
    api.state().sh_handler = Some(out_txt_handler());
    let seen = Seen::default();
    let truncated = ModelResponse {
        text: None,
        tool_calls: Vec::new(),
        stop_reason: kiln_harness::provider::StopReason::MaxTokens,
        usage: kiln_harness::provider::Usage::default(),
        provider_content: None,
    };
    let mut runs = 0;
    let out = temp.path().join("out");
    // alpha recovers once and passes; beta answers at once and fails.
    let summary = eval::run_suite(
        &api,
        &tasks,
        &options(&out, 1, true),
        || {
            runs += 1;
            let responses = if runs == 1 {
                vec![
                    truncated.clone(),
                    write_out(),
                    ModelResponse::final_text("done"),
                ]
            } else {
                vec![ModelResponse::final_text("done")]
            };
            Ok(Observing::new(&api, &seen, responses))
        },
        &CancellationToken::new(),
        &mut |_| Ok(()),
    )
    .await
    .expect("the suite runs");
    let results = read_lines(&out.join("results.jsonl"));
    assert_eq!(results, summary.results);
    assert_eq!(results[0].truncation_recoveries, 1);
    assert_eq!(results[0].steps, 3);
    assert_eq!(results[0].check.as_deref(), Some("passed"));
    assert_eq!(results[0].replay.as_deref(), Some("matched"));
    assert_eq!(results[1].truncation_recoveries, 0);
    // Always written, 0 included.
    let text = std::fs::read_to_string(out.join("results.jsonl")).expect("results");
    let lines = text.lines().collect::<Vec<_>>();
    assert!(
        lines[0].contains("\"truncation_recoveries\":1"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("\"truncation_recoveries\":0"),
        "{}",
        lines[1]
    );
    // A line written before the field existed still reads, as 0.
    let mut old: Value = serde_json::from_str(lines[0]).expect("line");
    old.as_object_mut()
        .expect("object")
        .remove("truncation_recoveries");
    let old: RunResult = serde_json::from_value(old).expect("old line");
    assert_eq!(old.truncation_recoveries, 0);
}

#[tokio::test]
async fn replay_in_the_runner_matches_or_diverges_at_the_check() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = two_tasks(&temp.path().join("tasks"));
    let tasks = vec![tasks[0].clone()];
    let api = FakeApi::default();
    api.state().sh_handler = Some(out_txt_handler());
    let seen = Seen::default();
    let summary = eval::run_suite(
        &api,
        &tasks,
        &options(&temp.path().join("same"), 1, true),
        || {
            Ok(Observing::new(
                &api,
                &seen,
                vec![write_out(), ModelResponse::final_text("done")],
            ))
        },
        &CancellationToken::new(),
        &mut |_| Ok(()),
    )
    .await
    .expect("the suite runs");
    assert_eq!(summary.results[0].replay.as_deref(), Some("matched"));
    let run = temp.path().join("same/runs/alpha/1");
    read_journal(&run.join("replay-journal.jsonl")).expect("the replay journal verifies");
    assert!(run.join("replay-audit.jsonl").is_file());
    assert_eq!(summary.report.tasks[0].replay_matched, 1);

    // The recording passes; the replayed check exits 1.
    let api = FakeApi::default();
    let calls = Arc::new(Mutex::new(0));
    let counted = calls.clone();
    api.state().sh_handler = Some(Arc::new(move |_: &[u8], _: &mut _| {
        let mut calls = counted.lock().expect("lock");
        *calls += 1;
        ShAnswer::exit(i32::from(*calls > 1))
    }));
    let summary = eval::run_suite(
        &api,
        &tasks,
        &options(&temp.path().join("different"), 1, true),
        || {
            Ok(Observing::new(
                &api,
                &seen,
                vec![write_out(), ModelResponse::final_text("done")],
            ))
        },
        &CancellationToken::new(),
        &mut |_| Ok(()),
    )
    .await
    .expect("the suite runs");
    assert_eq!(summary.results[0].check.as_deref(), Some("passed"));
    assert_eq!(summary.results[0].replay.as_deref(), Some("diverged_check"));
    assert_eq!(summary.report.tasks[0].replay_diverged, 1);
}

#[tokio::test]
async fn an_existing_out_is_refused_before_any_run() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = two_tasks(&temp.path().join("tasks"));
    let out = temp.path().join("out");
    std::fs::create_dir(&out).expect("out exists");
    let api = FakeApi::default();
    let error = eval::run_suite(
        &api,
        &tasks,
        &options(&out, 1, false),
        || Ok(ScriptedProvider::new([ModelResponse::final_text("done")])),
        &CancellationToken::new(),
        &mut |_| Ok(()),
    )
    .await
    .expect_err("refused");
    assert!(error.to_string().contains("must not exist"), "{error}");
    assert_eq!(api.state().health_calls, 0);
    assert!(api.state().create_requests.is_empty());
    assert_eq!(std::fs::read_dir(&out).expect("out").count(), 0);
}

#[tokio::test]
async fn an_unconfirmed_cleanup_stops_the_suite_with_a_partial_report() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = two_tasks(&temp.path().join("tasks"));
    let out = temp.path().join("out");
    let api = FakeApi::default();
    {
        let mut state = api.state();
        state.sh_handler = Some(out_txt_handler());
        state.delete_behavior = DeleteBehavior::AcknowledgeOnly;
    }
    let summary = eval::run_suite(
        &api,
        &tasks,
        &options(&out, 2, false),
        || Ok(ScriptedProvider::new([ModelResponse::final_text("done")])),
        &CancellationToken::new(),
        &mut |_| Ok(()),
    )
    .await
    .expect("the suite stops cleanly");
    assert!(
        summary
            .stopped
            .as_deref()
            .is_some_and(|reason| reason.contains("alpha trial 1")),
        "{:?}",
        summary.stopped
    );
    let results = read_lines(&out.join("results.jsonl"));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, "cleanup_failed");
    assert_eq!(results[0].work_outcome, "completed");
    assert!(results[0].error.is_some());
    assert_eq!(api.state().create_requests.len(), 1, "no second run");
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).expect("manifest"))
            .expect("manifest JSON");
    assert!(manifest.stopped.is_some());
    let report = eval::read_report(&out).expect("partial report");
    assert_eq!(report.overall.runs, 1);
    assert_eq!(report.tasks[1].runs, 0);
    assert_eq!(report.tasks[1].pass_rate, None);
    assert!(
        std::fs::read_to_string(out.join("report.md"))
            .expect("markdown")
            .contains("Stopped early")
    );
}

#[tokio::test]
async fn cancellation_stops_after_the_current_run() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = two_tasks(&temp.path().join("tasks"));
    let out = temp.path().join("out");
    let api = FakeApi::default();
    api.state().sh_handler = Some(out_txt_handler());
    let cancel = CancellationToken::new();
    let summary = eval::run_suite(
        &api,
        &tasks,
        &options(&out, 2, false),
        || Ok(ScriptedProvider::new([ModelResponse::final_text("done")])),
        &cancel,
        &mut |_| {
            cancel.cancel();
            Ok(())
        },
    )
    .await
    .expect("stops");
    assert_eq!(summary.stopped.as_deref(), Some("canceled"));
    assert_eq!(summary.results.len(), 1);
    assert_eq!(api.state().deleted.len(), 1);
    assert!(out.join("report.json").is_file());
}

#[test]
fn the_runner_options_are_checked() {
    assert!(eval::validate_trials_and_label(0, None).is_err());
    assert!(eval::validate_trials_and_label(101, None).is_err());
    assert!(eval::validate_trials_and_label(100, Some(&"x".repeat(128))).is_ok());
    assert!(eval::validate_trials_and_label(1, Some(&"x".repeat(129))).is_err());
    assert!(eval::validate_trials_and_label(1, Some("a\nb")).is_err());
    for refused in [0, 9] {
        let error = eval::validate_concurrency(refused).expect_err("out of range");
        assert_eq!(error.to_string(), "--concurrency must be between 1 and 8");
    }
    for accepted in [1, 8] {
        eval::validate_concurrency(accepted).expect("in range");
    }
}

// ---------------------------------------------------------------------------
// Runs side by side

/// How long a concurrent suite may take before the test fails instead of
/// hanging on a scheduling bug.
const SUITE_LIMIT: Duration = Duration::from_secs(30);

/// A daemon of many sandboxes, for runs side by side: each create gets a
/// fake of its own under an ID of its own, so sessions share neither a
/// workspace nor a listing. Calls that name a sandbox go to its fake;
/// `GetExecution`, which names none, goes to the sandbox this connection
/// created last, since a connection serves one run at a time.
#[derive(Clone)]
struct Fleet {
    shared: Arc<Mutex<FleetState>>,
    /// The sandbox this connection created last.
    current: Arc<Mutex<Option<FakeApi>>>,
}

struct FleetState {
    /// `/bin/sh` in every sandbox.
    handler: ShHandler,
    /// Every sandbox's ID and fake, in creation order.
    sandboxes: Vec<(String, FakeApi)>,
    reconnects: u32,
    /// The first this many sandboxes asked to be deleted only acknowledge
    /// it, so their absence is never confirmed.
    unconfirmed_deletes: u32,
    /// The sandboxes left unconfirmed.
    unconfirmed: Vec<String>,
    /// Like kilnd's Health lane: at most this many Health calls at once,
    /// each taking `health_delay`; more are refused `RESOURCE_EXHAUSTED`.
    health_cap: Option<u32>,
    health_delay: Duration,
    health_in_flight: u32,
    health_refused: u32,
}

impl Fleet {
    fn new(handler: ShHandler) -> Self {
        Self {
            shared: Arc::new(Mutex::new(FleetState {
                handler,
                sandboxes: Vec::new(),
                reconnects: 0,
                unconfirmed_deletes: 0,
                unconfirmed: Vec::new(),
                health_cap: None,
                health_delay: Duration::ZERO,
                health_in_flight: 0,
                health_refused: 0,
            })),
            current: Arc::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FleetState> {
        self.shared.lock().expect("fleet lock")
    }

    fn sandbox(&self, id: &str) -> FakeApi {
        self.state()
            .sandboxes
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, api)| api.clone())
            .expect("a sandbox this fleet created")
    }

    /// Each sandbox's rows created and deletions confirmed, in creation
    /// order.
    fn lifecycles(&self) -> Vec<(usize, usize)> {
        self.state()
            .sandboxes
            .iter()
            .map(|(_, api)| {
                let state = api.state();
                (state.creates.len(), state.deleted.len())
            })
            .collect()
    }
}

#[async_trait]
impl SandboxApi for Fleet {
    async fn health(&self) -> Result<Health, ApiError> {
        let delay = {
            let mut state = self.state();
            if state
                .health_cap
                .is_some_and(|cap| state.health_in_flight >= cap)
            {
                state.health_refused += 1;
                return Err(api_error(
                    tonic::Code::ResourceExhausted,
                    "Health RPC capacity reached",
                ));
            }
            state.health_in_flight += 1;
            state.health_delay
        };
        tokio::time::sleep(delay).await;
        self.state().health_in_flight -= 1;
        FakeApi::default().health().await
    }

    async fn create(&self, request: SandboxRequest) -> Result<Sandbox, ApiError> {
        let api = FakeApi::default();
        let id = {
            let mut state = self.state();
            api.state().sh_handler = Some(state.handler.clone());
            let id = format!("sb-{:016x}", state.sandboxes.len() + 1);
            state.sandboxes.push((id.clone(), api.clone()));
            id
        };
        *self.current.lock().expect("connection lock") = Some(api.clone());
        let mut sandbox = api.create(request).await?;
        sandbox.id = id;
        Ok(sandbox)
    }

    async fn exec(&self, request: SandboxExec) -> Result<Execution, ApiError> {
        self.sandbox(&request.sandbox_id).exec(request).await
    }

    async fn exec_stream(
        &self,
        request: SandboxExec,
        observer: &mut (dyn StreamObserver + Send),
    ) -> Result<StreamedExec, ApiError> {
        self.sandbox(&request.sandbox_id)
            .exec_stream(request, observer)
            .await
    }

    async fn recent_executions(&self, sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        self.sandbox(sandbox_id).recent_executions(sandbox_id).await
    }

    async fn delete(
        &self,
        sandbox_id: &str,
        release_request_id: bool,
    ) -> Result<Deleted, ApiError> {
        let api = self.sandbox(sandbox_id);
        {
            let mut state = self.state();
            if state.unconfirmed_deletes > 0 && !state.unconfirmed.iter().any(|id| id == sandbox_id)
            {
                state.unconfirmed_deletes -= 1;
                state.unconfirmed.push(sandbox_id.to_string());
                api.state().delete_behavior = DeleteBehavior::AcknowledgeOnly;
            }
        }
        api.delete(sandbox_id, release_request_id).await
    }

    async fn renew(&self, sandbox_id: &str) -> Result<(), ApiError> {
        self.sandbox(sandbox_id).renew(sandbox_id).await
    }

    async fn get_execution(&self, execution_id: &str) -> Result<Execution, ApiError> {
        let current = self
            .current
            .lock()
            .expect("connection lock")
            .clone()
            .expect("a sandbox created on this connection");
        current.get_execution(execution_id).await
    }

    async fn final_check(&self, sandbox_id: &str) -> Result<FinalCheck, ApiError> {
        self.sandbox(sandbox_id).final_check(sandbox_id).await
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        let sandboxes = self.state().sandboxes.clone();
        let mut listed = Vec::new();
        for (id, api) in sandboxes {
            for mut sandbox in api.non_deleted_sandboxes().await? {
                sandbox.id.clone_from(&id);
                listed.push(sandbox);
            }
        }
        Ok(listed)
    }
}

/// A new connection shares the fleet but not the last sandbox created.
#[async_trait]
impl Reconnect for Fleet {
    async fn reconnect(&self) -> Result<Self, ApiError> {
        self.state().reconnects += 1;
        Ok(Self {
            shared: self.shared.clone(),
            current: Arc::default(),
        })
    }
}

/// A scripted model for runs side by side. With a `gate`, its first
/// request waits there for the other gated sessions; with `entered`, it
/// says so and then never answers, until its session is canceled.
struct SideBySide {
    inner: ScriptedProvider,
    gate: Option<Arc<tokio::sync::Barrier>>,
    entered: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}

impl SideBySide {
    fn answering(responses: Vec<ModelResponse>) -> Self {
        Self {
            inner: ScriptedProvider::new(responses),
            gate: None,
            entered: None,
        }
    }

    fn gated(responses: Vec<ModelResponse>, gate: &Arc<tokio::sync::Barrier>) -> Self {
        Self {
            gate: Some(gate.clone()),
            ..Self::answering(responses)
        }
    }

    fn holding(entered: &tokio::sync::mpsc::UnboundedSender<()>) -> Self {
        Self {
            entered: Some(entered.clone()),
            ..Self::answering(Vec::new())
        }
    }
}

#[async_trait]
impl Provider for SideBySide {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "scripted".to_string(),
            model: "observing".to_string(),
        }
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        // Announced before the gate, so a peer that the gate releases and
        // the suite then cancels has still been seen to enter.
        if let Some(entered) = &self.entered {
            entered.send(()).expect("the test listens");
        }
        if let Some(gate) = self.gate.take() {
            gate.wait().await;
        }
        if self.entered.is_some() {
            std::future::pending::<()>().await;
        }
        self.inner.complete(request).await
    }
}

fn at(out: &Path, trials: u32, concurrency: u32) -> SuiteOptions {
    SuiteOptions {
        concurrency,
        ..options(out, trials, false)
    }
}

fn one_task(root: &Path) -> Vec<EvalTask> {
    make_task(root, "alpha", &task_json("alpha"), &[]);
    load_tasks(root, &[], &config()).expect("loads")
}

fn read_manifest(out: &Path) -> Manifest {
    serde_json::from_slice(&std::fs::read(out.join("manifest.json")).expect("manifest"))
        .expect("manifest JSON")
}

fn by_task_and_trial(results: &[RunResult]) -> Vec<RunResult> {
    let mut sorted = results.to_vec();
    sorted.sort_by(|left, right| (&left.task, left.trial).cmp(&(&right.task, right.trial)));
    sorted
}

#[tokio::test]
async fn concurrent_runs_overlap_and_report_in_canonical_order() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = two_tasks(&temp.path().join("tasks"));
    let out = temp.path().join("out");
    let api = Fleet::new(out_txt_handler());
    // The first four sessions' first requests meet here, which opens only
    // when four run at once.
    let gate = Arc::new(tokio::sync::Barrier::new(4));
    let mut runs = 0;
    let mut progress = Vec::new();
    // alpha's three runs write out.txt and pass; beta's answer at once and fail.
    let summary = tokio::time::timeout(
        SUITE_LIMIT,
        eval::run_suite(
            &api,
            &tasks,
            &at(&out, 3, 4),
            || {
                runs += 1;
                let responses = if runs <= 3 {
                    vec![write_out(), ModelResponse::final_text("done")]
                } else {
                    vec![ModelResponse::final_text("done")]
                };
                Ok(if runs <= 4 {
                    SideBySide::gated(responses, &gate)
                } else {
                    SideBySide::answering(responses)
                })
            },
            &CancellationToken::new(),
            &mut |result| {
                progress.push(result.clone());
                Ok(())
            },
        ),
    )
    .await
    .expect("four sessions ran at once")
    .expect("the suite runs");

    assert_eq!(summary.stopped, None);
    assert_eq!(
        summary
            .results
            .iter()
            .map(|result| (result.task.as_str(), result.trial, result.check.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("alpha", 1, Some("passed")),
            ("alpha", 2, Some("passed")),
            ("alpha", 3, Some("passed")),
            ("beta", 1, Some("failed")),
            ("beta", 2, Some("failed")),
            ("beta", 3, Some("failed")),
        ]
    );
    // Lines are written as runs end, and progress sees them in that order.
    let lines = read_lines(&out.join("results.jsonl"));
    assert_eq!(lines, progress);
    assert_eq!(by_task_and_trial(&lines), summary.results);
    for result in &summary.results {
        assert_eq!(result.outcome, "completed");
        assert_eq!(result.error, None);
        let run = out
            .join("runs")
            .join(&result.task)
            .join(result.trial.to_string());
        read_journal(&run.join("journal.jsonl")).expect("each journal verifies");
    }
    // Every sandbox was its own and every deletion was confirmed.
    assert_eq!(api.lifecycles(), vec![(1, 1); 6]);
    // Four connections at most: the suite's and three more, reused.
    assert_eq!(api.state().reconnects, 3);

    let report = eval::read_report(&out).expect("report");
    assert_eq!(report, summary.report);
    assert_eq!(report.concurrency, 4);
    assert_eq!((report.tasks[0].runs, report.tasks[0].passed), (3, 3));
    assert_eq!((report.tasks[1].runs, report.tasks[1].passed), (3, 0));
    assert_eq!(read_manifest(&out).concurrency, 4);
    let markdown = std::fs::read_to_string(out.join("report.md")).expect("markdown");
    assert!(
        markdown.contains("- Trials per task: 3\n- Runs at a time: 4\n"),
        "{markdown}"
    );
}

#[tokio::test]
async fn an_unconfirmed_cleanup_cancels_and_drains_the_other_runs() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = one_task(&temp.path().join("tasks"));
    let out = temp.path().join("out");
    let api = Fleet::new(out_txt_handler());
    api.state().unconfirmed_deletes = 1;
    let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
    let mut runs = 0;
    // Trial 1 answers at once and its deletion is never confirmed; trials
    // 2 and 3 wait for their model until they are canceled.
    let summary = tokio::time::timeout(
        SUITE_LIMIT,
        eval::run_suite(
            &api,
            &tasks,
            &at(&out, 5, 3),
            || {
                runs += 1;
                Ok(if runs == 1 {
                    SideBySide::answering(vec![ModelResponse::final_text("done")])
                } else {
                    SideBySide::holding(&entered)
                })
            },
            &CancellationToken::new(),
            &mut |_| Ok(()),
        ),
    )
    .await
    .expect("the suite drains")
    .expect("the suite stops cleanly");

    assert_eq!(
        summary.stopped.as_deref(),
        Some("task alpha trial 1: sandbox deletion was not confirmed")
    );
    assert_eq!(runs, 3, "trials 4 and 5 never start");
    let mut waited = 0;
    while entries.try_recv().is_ok() {
        waited += 1;
    }
    assert_eq!(waited, 2);
    let lines = by_task_and_trial(&read_lines(&out.join("results.jsonl")));
    assert_eq!(
        lines
            .iter()
            .map(|result| (result.trial, result.outcome.as_str()))
            .collect::<Vec<_>>(),
        [(1, "cleanup_failed"), (2, "canceled"), (3, "canceled")]
    );
    assert_eq!(lines, summary.results);
    // Trial 1's sandbox was left; the canceled runs cleaned up their own.
    let mut lifecycles = api.lifecycles();
    lifecycles.sort_unstable();
    assert_eq!(lifecycles, [(1, 0), (1, 1), (1, 1)]);
    assert_eq!(api.state().unconfirmed.len(), 1);
    assert_eq!(read_manifest(&out).stopped, summary.stopped);
    let report = eval::read_report(&out).expect("partial report");
    assert_eq!(report.overall.runs, 3);
    assert_eq!(report.stopped, summary.stopped);
}

#[tokio::test]
async fn cancellation_reaches_every_in_flight_run() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = one_task(&temp.path().join("tasks"));
    let out = temp.path().join("out");
    let api = Fleet::new(out_txt_handler());
    let cancel = CancellationToken::new();
    let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
    let options = at(&out, 6, 3);
    let mut progress = |_: &RunResult| Ok(());
    let suite = eval::run_suite(
        &api,
        &tasks,
        &options,
        || Ok(SideBySide::holding(&entered)),
        &cancel,
        &mut progress,
    );
    // Cancel once three sessions wait for their model at once.
    let canceller = async {
        for _ in 0..3 {
            entries.recv().await.expect("a session waits");
        }
        cancel.cancel();
    };
    let (summary, ()) = tokio::time::timeout(SUITE_LIMIT, async { tokio::join!(suite, canceller) })
        .await
        .expect("the suite drains");
    let summary = summary.expect("the suite stops cleanly");

    assert_eq!(summary.stopped.as_deref(), Some("canceled"));
    let lines = read_lines(&out.join("results.jsonl"));
    assert_eq!(lines.len(), 3);
    assert!(
        lines.iter().all(|result| result.outcome == "canceled"),
        "{lines:?}"
    );
    assert_eq!(by_task_and_trial(&lines), summary.results);
    assert_eq!(
        summary
            .results
            .iter()
            .map(|result| result.trial)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(api.lifecycles(), vec![(1, 1); 3]);
    assert_eq!(eval::read_report(&out).expect("report").overall.runs, 3);
}

#[tokio::test]
async fn a_failed_progress_output_stops_the_suite_and_drains_the_other_runs() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = one_task(&temp.path().join("tasks"));
    let out = temp.path().join("out");
    let api = Fleet::new(out_txt_handler());
    let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
    let mut runs = 0;
    let mut calls = 0;
    // Trial 1 answers once trials 2 and 3 wait for their model; its
    // progress line cannot be written.
    let gate = Arc::new(tokio::sync::Barrier::new(3));
    let summary = tokio::time::timeout(
        SUITE_LIMIT,
        eval::run_suite(
            &api,
            &tasks,
            &at(&out, 5, 3),
            || {
                runs += 1;
                Ok(if runs == 1 {
                    SideBySide::gated(vec![ModelResponse::final_text("done")], &gate)
                } else {
                    SideBySide {
                        gate: Some(gate.clone()),
                        ..SideBySide::holding(&entered)
                    }
                })
            },
            &CancellationToken::new(),
            &mut |_| {
                calls += 1;
                Err("closed".to_string())
            },
        ),
    )
    .await
    .expect("the suite drains")
    .expect("the suite stops cleanly");

    assert_eq!(summary.stopped.as_deref(), Some("progress output: closed"));
    assert_eq!(calls, 1, "progress is not called after it failed");
    assert_eq!(runs, 3, "trials 4 and 5 never start");
    let mut waited = 0;
    while entries.try_recv().is_ok() {
        waited += 1;
    }
    assert_eq!(waited, 2);
    // Every line is still written, and every sandbox cleaned up.
    let lines = by_task_and_trial(&read_lines(&out.join("results.jsonl")));
    assert_eq!(
        lines
            .iter()
            .map(|result| (result.trial, result.outcome.as_str()))
            .collect::<Vec<_>>(),
        [(1, "completed"), (2, "canceled"), (3, "canceled")]
    );
    assert_eq!(lines, summary.results);
    assert_eq!(api.lifecycles(), vec![(1, 1); 3]);
    assert_eq!(
        eval::read_report(&out).expect("report").stopped,
        summary.stopped
    );
}

#[tokio::test]
async fn a_journal_failure_stops_the_suite_and_drains_the_other_runs() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = one_task(&temp.path().join("tasks"));
    let out = temp.path().join("out");
    let api = Fleet::new(out_txt_handler());
    let (entered, _entries) = tokio::sync::mpsc::unbounded_channel();
    let mut runs = 0;
    // Trial 1's model answers with more than one journal line may hold, so
    // its journal refuses the record mid-run, once trials 2 and 3 wait.
    let gate = Arc::new(tokio::sync::Barrier::new(3));
    let huge = "x".repeat(MAX_JOURNAL_LINE_BYTES + 1);
    let mut options = at(&out, 5, 3);
    options.replay = true;
    let summary = tokio::time::timeout(
        SUITE_LIMIT,
        eval::run_suite(
            &api,
            &tasks,
            &options,
            || {
                runs += 1;
                Ok(if runs == 1 {
                    SideBySide::gated(vec![ModelResponse::final_text(huge.clone())], &gate)
                } else {
                    SideBySide {
                        gate: Some(gate.clone()),
                        ..SideBySide::holding(&entered)
                    }
                })
            },
            &CancellationToken::new(),
            &mut |_| Ok(()),
        ),
    )
    .await
    .expect("the suite drains")
    .expect("the suite stops cleanly");

    let stopped = summary.stopped.expect("stopped");
    assert!(
        stopped.starts_with("task alpha trial 1: run outputs: journal line ")
            && stopped.contains("exceeds"),
        "{stopped}"
    );
    assert_eq!(runs, 3, "trials 4 and 5 never start");
    let lines = by_task_and_trial(&read_lines(&out.join("results.jsonl")));
    assert_eq!(
        lines
            .iter()
            .map(|result| (
                result.trial,
                result.outcome.as_str(),
                result.replay.as_deref()
            ))
            .collect::<Vec<_>>(),
        [
            (1, "failed", None),
            (2, "canceled", None),
            (3, "canceled", None)
        ]
    );
    assert!(
        lines[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("exceeds")),
        "{:?}",
        lines[0].error
    );
    // No replay ran: three sandboxes, each cleaned up.
    assert_eq!(api.lifecycles(), vec![(1, 1); 3]);
}

/// Eight runs that start together meet kilnd's Health lane, which takes 4
/// calls at once; their discovery waits for it instead of failing, and a
/// second wave does the same.
#[tokio::test]
async fn runs_that_meet_the_health_lane_wait_for_it() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = one_task(&temp.path().join("tasks"));
    let out = temp.path().join("out");
    let api = Fleet::new(out_txt_handler());
    {
        let mut state = api.state();
        state.health_cap = Some(4);
        state.health_delay = Duration::from_millis(20);
    }
    let summary = tokio::time::timeout(
        SUITE_LIMIT,
        eval::run_suite(
            &api,
            &tasks,
            &at(&out, 16, 8),
            || {
                Ok(SideBySide::answering(vec![
                    write_out(),
                    ModelResponse::final_text("done"),
                ]))
            },
            &CancellationToken::new(),
            &mut |_| Ok(()),
        ),
    )
    .await
    .expect("the suite finishes")
    .expect("the suite runs");

    assert_eq!(summary.stopped, None);
    assert_eq!(summary.results.len(), 16);
    assert!(
        summary.results.iter().all(RunResult::passed),
        "{:?}",
        summary.results
    );
    assert!(api.state().health_refused > 0, "the lane was full");
    assert_eq!(api.lifecycles(), vec![(1, 1); 16]);
}

#[tokio::test]
async fn a_failed_connection_stops_the_suite_without_a_line() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = one_task(&temp.path().join("tasks"));
    let out = temp.path().join("out");
    let api = FakeApi::default();
    api.state().reconnect_error = Some(api_error(tonic::Code::Unavailable, "no route to kilnd"));
    let (entered, _entries) = tokio::sync::mpsc::unbounded_channel();
    let mut runs = 0;
    // Trial 1 runs on the suite's own connection and waits for its model;
    // trial 2 needs a new connection, which fails.
    let summary = tokio::time::timeout(
        SUITE_LIMIT,
        eval::run_suite(
            &api,
            &tasks,
            &at(&out, 3, 2),
            || {
                runs += 1;
                Ok(SideBySide::holding(&entered))
            },
            &CancellationToken::new(),
            &mut |_| Ok(()),
        ),
    )
    .await
    .expect("the suite drains")
    .expect("the suite stops cleanly");

    let stopped = summary.stopped.expect("stopped");
    assert!(
        stopped.starts_with("task alpha trial 2: connect: ")
            && stopped.contains("no route to kilnd"),
        "{stopped}"
    );
    assert_eq!(runs, 2, "trial 3 never starts");
    let lines = read_lines(&out.join("results.jsonl"));
    assert_eq!(
        lines
            .iter()
            .map(|result| (result.trial, result.outcome.as_str()))
            .collect::<Vec<_>>(),
        [(1, "canceled")]
    );
    assert!(!out.join("runs/alpha/2").exists());
    let state = api.state();
    assert_eq!(state.reconnects, 1);
    assert_eq!(state.creates.len(), 1);
    assert_eq!(state.deleted.len(), 1);
    assert!(out.join("report.json").is_file());
}

// ---------------------------------------------------------------------------
// Reports

fn result(
    task: &str,
    trial: u32,
    check: Option<&str>,
    steps: u32,
    tokens: (u64, u64),
) -> RunResult {
    RunResult {
        task: task.to_string(),
        trial,
        session_id: Some(format!("hs-{task}-{trial}")),
        outcome: "completed".to_string(),
        work_outcome: "completed".to_string(),
        check: check.map(str::to_string),
        check_exit_code: match check {
            Some("passed") => Some(0),
            Some("failed") => Some(1),
            _ => None,
        },
        steps,
        tool_calls: steps.saturating_sub(1),
        input_tokens: tokens.0,
        output_tokens: tokens.1,
        cache_read_tokens: None,
        cache_write_tokens: None,
        wall_ms: u64::from(steps) * 1000,
        truncation_recoveries: 0,
        replay: None,
        error: None,
    }
}

fn manifest(tasks: &[&str], trials: u32) -> Manifest {
    Manifest {
        schema: 1,
        harness_version: "0.1.0".to_string(),
        label: None,
        provider: "openai-compat".to_string(),
        model: "local-coder".to_string(),
        base_url_host: Some("192.168.1.20".to_string()),
        tasks: strings(tasks),
        trials,
        concurrency: 1,
        max_run_secs: None,
        effort: None,
        max_output_tokens: None,
        replay: true,
        started_unix_ms: 1,
        ended_unix_ms: 2,
        features: Vec::new(),
        stopped: None,
    }
}

#[test]
fn the_report_aggregates_per_task_and_overall() {
    let mut failed_run = result("b", 2, None, 1, (5, 1));
    failed_run.outcome = "deadline".to_string();
    failed_run.work_outcome = "deadline".to_string();
    let mut replayed = result("a", 1, Some("passed"), 2, (100, 10));
    replayed.replay = Some("matched".to_string());
    let mut diverged = result("a", 2, Some("passed"), 4, (300, 30));
    diverged.replay = Some("diverged_turn:2".to_string());
    let mut checked = result("a", 3, Some("failed"), 3, (200, 20));
    checked.replay = Some("diverged_check".to_string());
    let results = vec![
        replayed,
        diverged,
        checked,
        result("b", 1, Some("error"), 1, (5, 1)),
        failed_run,
        result("b", 3, Some("passed"), 2, (5, 1)),
        result("ignored", 1, Some("passed"), 1, (1, 1)),
    ];
    let report = eval::report(&results, &manifest(&["a", "b", "c"], 3));

    let a = &report.tasks[0];
    assert_eq!((a.runs, a.passed), (3, 2));
    assert_eq!(a.pass_rate, Some(2.0 / 3.0));
    assert_eq!(a.outcomes, BTreeMap::from([("completed".to_string(), 3)]));
    assert_eq!(
        a.steps,
        Stat {
            mean: Some(3.0),
            median: Some(3.0)
        }
    );
    assert_eq!(a.input_tokens.mean, Some(200.0));
    assert_eq!(
        (a.replay_matched, a.replay_diverged, a.replay_failed),
        (1, 2, 0)
    );
    let b = &report.tasks[1];
    assert_eq!((b.runs, b.passed), (3, 1));
    assert_eq!(
        b.outcomes,
        BTreeMap::from([("completed".to_string(), 2), ("deadline".to_string(), 1)])
    );
    let c = &report.tasks[2];
    assert_eq!((c.runs, c.pass_rate), (0, None));
    assert_eq!(c.steps.mean, None);

    let overall = &report.overall;
    assert_eq!((overall.tasks, overall.runs, overall.passed), (3, 6, 3));
    // The mean of 2/3 and 1/3; the task without runs is left out.
    assert!((overall.macro_pass_rate.expect("rate") - 0.5).abs() < 1e-12);
    assert_eq!(overall.pass_at_1, overall.macro_pass_rate);
    assert_eq!(overall.trials, 3);
    assert_eq!(overall.totals.steps, 13);
    assert_eq!(overall.totals.input_tokens, 615);
    assert_eq!(overall.means.output_tokens, Some(63.0 / 6.0));

    let markdown = eval::report_markdown(&report);
    assert!(markdown.contains("Provider: openai-compat, model: local-coder"));
    assert!(
        markdown.contains("| a | 3 | 2 | 66.7% | completed 3 | 3.0 / 3.0 |"),
        "{markdown}"
    );
    assert!(markdown.contains("| c | 0 | 0 | n/a |"), "{markdown}");
    assert!(
        markdown.contains("pass@1 from 3 trial(s) per task: 50.0%"),
        "{markdown}"
    );
}

#[test]
fn medians_of_even_counts_average_the_middle_pair() {
    assert_eq!(
        Stat::of(&[4, 1, 3, 2]),
        Stat {
            mean: Some(2.5),
            median: Some(2.5)
        }
    );
    assert_eq!(
        Stat::of(&[]),
        Stat {
            mean: None,
            median: None
        }
    );
}

#[test]
fn compare_shows_deltas_and_one_sided_tasks() {
    let base = eval::report(
        &[
            result("a", 1, Some("passed"), 2, (100, 10)),
            result("a", 2, Some("failed"), 4, (300, 30)),
            result("gone", 1, Some("passed"), 1, (1, 1)),
        ],
        &manifest(&["a", "gone"], 2),
    );
    let mut new_manifest = manifest(&["a", "fresh"], 2);
    new_manifest.label = Some("new prompt".to_string());
    let new = eval::report(
        &[
            result("a", 1, Some("passed"), 2, (50, 5)),
            result("a", 2, Some("passed"), 2, (50, 5)),
            result("fresh", 1, Some("failed"), 3, (10, 1)),
        ],
        &new_manifest,
    );
    let text = eval::compare(&base, &new);
    assert!(
        text.contains(
            "| a | 50.0% → 100.0% | +50.0 pp | 3.0 → 2.0 | -1.0 | 220.0 → 55.0 | -165.0 |"
        ),
        "{text}"
    );
    assert!(
        text.contains("| fresh (only in new) | n/a → 0.0% | n/a |"),
        "{text}"
    );
    assert!(
        text.contains("| gone (only in base) | 100.0% → n/a | n/a |"),
        "{text}"
    );
    assert!(
        text.contains("New: openai-compat local-coder (new prompt), 2 trial(s)"),
        "{text}"
    );
    assert!(
        text.contains("Overall macro pass rate: 75.0% → 50.0% (-25.0 pp)"),
        "{text}"
    );
    let rows = text
        .lines()
        .filter(|line| line.starts_with("| ") && !line.starts_with("| Task"))
        .map(|line| line.split(' ').nth(1).expect("task"))
        .collect::<Vec<_>>();
    assert_eq!(rows, ["a", "fresh", "gone"], "sorted union");
}

// ---------------------------------------------------------------------------
// CLI

fn cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
        .args(args)
        .output()
        .expect("run kiln-harness")
}

#[test]
fn reports_without_concurrency_read_as_one() {
    let one = manifest(&["a"], 1);
    let mut four = manifest(&["a"], 1);
    four.concurrency = 4;
    let runs = [result("a", 1, Some("passed"), 2, (10, 1))];
    let base = eval::report(&runs, &one);
    let new = eval::report(&runs, &four);
    assert_eq!((base.concurrency, new.concurrency), (1, 4));

    // A manifest and a report written before the field existed.
    let mut older = serde_json::to_value(&four).expect("manifest JSON");
    older
        .as_object_mut()
        .expect("object")
        .remove("concurrency")
        .expect("listed");
    let older: Manifest = serde_json::from_value(older).expect("an older manifest parses");
    assert_eq!(older.concurrency, 1);
    let temp = common::private_tempdir().expect("tempdir");
    let mut older = serde_json::to_value(&new).expect("report JSON");
    older
        .as_object_mut()
        .expect("object")
        .remove("concurrency")
        .expect("listed");
    write(
        &temp.path().join("report.json"),
        older.to_string().as_bytes(),
    );
    assert_eq!(
        eval::read_report(temp.path())
            .expect("an older report reads")
            .concurrency,
        1
    );

    let markdown = eval::report_markdown(&new);
    assert!(
        markdown.contains("- Trials per task: 1\n- Runs at a time: 4\n"),
        "{markdown}"
    );
    let differing = eval::compare(&base, &new);
    assert!(
        differing.ends_with(
            ").\n\nRuns at a time differ (1 → 4); wall times are not directly comparable.\n"
        ),
        "{differing}"
    );
    for same in [eval::compare(&base, &base), eval::compare(&new, &new)] {
        assert!(!same.contains("Runs at a time"), "{same}");
    }
}

#[test]
fn eval_run_help_lists_the_shared_flags() {
    let output = cli(&["eval", "run", "--help"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for flag in [
        "--provider",
        "--base-url",
        "--model",
        "--api-key-env",
        "--temperature",
        "--seed",
        "--effort",
        "--fallbacks",
        "--strict-tools",
        "--max-steps",
        "--max-tool-calls",
        "--max-tool-calls-per-turn",
        "--session-timeout-secs",
        "--tool-timeout-secs",
        "--sandbox-idle-timeout-secs",
        "--sandbox-lifetime-margin-secs",
        "--max-output-tokens",
        "--max-total-input-tokens",
        "--max-total-output-tokens",
        "--max-total-output-bytes",
        "--tasks",
        "--task",
        "--trials",
        "--concurrency",
        "--max-run-secs",
        "--out",
        "--label",
        "--replay",
    ] {
        assert!(text.contains(flag), "{flag}: {text}");
    }
    assert!(!text.contains("--prompt"), "{text}");
    for command in [
        &["eval", "--help"][..],
        &["eval", "selftest", "--help"],
        &["eval", "compare", "--help"],
    ] {
        assert!(cli(command).status.success(), "{command:?}");
    }
}

#[test]
fn eval_run_refuses_an_out_of_range_concurrency_before_contacting_anything() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = Path::new(env!("CARGO_MANIFEST_DIR")).join("evals/tasks");
    let out = temp.path().join("out");
    for concurrency in ["0", "9"] {
        let output = cli(&[
            "--socket",
            temp.path().join("no.sock").to_str().expect("utf-8"),
            "eval",
            "run",
            "--provider",
            "openai-compat",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "m",
            "--tasks",
            tasks.to_str().expect("utf-8"),
            "--concurrency",
            concurrency,
            "--out",
            out.to_str().expect("utf-8"),
        ]);
        assert!(!output.status.success(), "{concurrency}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--concurrency must be between 1 and 8"),
            "{stderr}"
        );
        assert!(!stderr.contains("no.sock"), "{stderr}");
        assert!(!out.exists());
    }
}

#[test]
fn eval_run_refuses_an_existing_out_before_contacting_anything() {
    let temp = common::private_tempdir().expect("tempdir");
    let out = temp.path().join("out");
    std::fs::create_dir(&out).expect("out");
    let tasks = Path::new(env!("CARGO_MANIFEST_DIR")).join("evals/tasks");
    let output = cli(&[
        "--socket",
        temp.path().join("no.sock").to_str().expect("utf-8"),
        "eval",
        "run",
        "--provider",
        "openai-compat",
        "--base-url",
        "http://127.0.0.1:9/v1",
        "--model",
        "m",
        "--tasks",
        tasks.to_str().expect("utf-8"),
        "--out",
        out.to_str().expect("utf-8"),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--out must not exist"), "{stderr}");

    let output = cli(&[
        "eval",
        "run",
        "--provider",
        "openai-compat",
        "--base-url",
        "http://127.0.0.1:9/v1",
        "--model",
        "m",
        "--tasks",
        tasks.to_str().expect("utf-8"),
        "--task",
        "no-such-task",
        "--out",
        temp.path().join("new").to_str().expect("utf-8"),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no-such-task"), "{stderr}");
    assert!(!temp.path().join("new").exists());
}

#[test]
fn eval_run_and_selftest_refuse_an_empty_suite_before_contacting_anything() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = temp.path().join("tasks");
    std::fs::create_dir(&tasks).expect("tasks");
    let socket = temp.path().join("no.sock");
    let socket = socket.to_str().expect("utf-8");
    let tasks = tasks.to_str().expect("utf-8");
    let out = temp.path().join("out");
    let out = out.to_str().expect("utf-8");
    let run = [
        "--socket",
        socket,
        "eval",
        "run",
        "--provider",
        "openai-compat",
        "--base-url",
        "http://127.0.0.1:9/v1",
        "--model",
        "m",
        "--tasks",
        tasks,
        "--out",
        out,
    ];
    let selftest = [
        "--socket", socket, "eval", "selftest", "--tasks", tasks, "--out", out,
    ];
    for args in [&run[..], &selftest[..]] {
        let output = cli(args);
        assert!(!output.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("{tasks} holds no tasks")),
            "{stderr}"
        );
        assert!(!temp.path().join("out").exists(), "{args:?}");
    }
}

#[test]
fn eval_compare_prints_markdown_for_two_report_directories() {
    let temp = common::private_tempdir().expect("tempdir");
    for (name, check) in [("base", "failed"), ("new", "passed")] {
        let report = eval::report(
            &[result("a", 1, Some(check), 2, (10, 1))],
            &manifest(&["a"], 1),
        );
        write(
            &temp.path().join(name).join("report.json"),
            &serde_json::to_vec(&report).expect("report"),
        );
    }
    let output = cli(&[
        "eval",
        "compare",
        temp.path().join("base").to_str().expect("utf-8"),
        temp.path().join("new").to_str().expect("utf-8"),
    ]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("| a | 0.0% → 100.0% | +100.0 pp |"), "{text}");
}

#[tokio::test]
async fn the_selftest_proves_a_task_and_catches_a_broken_one() {
    let temp = common::private_tempdir().expect("tempdir");
    let root = temp.path().join("tasks");
    make_task(&root, "good", &task_json("good"), &[]);
    // A seed that already solves the task: the bare check passes.
    make_task(
        &root,
        "solved-seed",
        &task_json("solved-seed"),
        &[("out.txt", b"done\n")],
    );
    let tasks = load_tasks(&root, &[], &eval::selftest_config()).expect("loads");
    let api = FakeApi::default();
    api.state().sh_handler = Some(out_txt_handler());
    let mut lines = Vec::new();
    let summary = eval::selftest(
        &api,
        &tasks,
        &eval::selftest_config(),
        &temp.path().join("out"),
        &CancellationToken::new(),
        &mut |result| lines.push(result.clone()),
    )
    .await
    .expect("runs");
    assert!(!summary.ok());
    assert_eq!(summary.results, lines);
    assert_eq!(summary.results[0].problem, None);
    assert!(
        summary.results[1]
            .problem
            .as_deref()
            .is_some_and(|problem| problem.contains("passed on the bare seed")),
        "{:?}",
        summary.results[1]
    );
    for kind in ["bare", "solved"] {
        read_journal(
            &temp
                .path()
                .join("out/good")
                .join(kind)
                .join("journal.jsonl"),
        )
        .expect("selftest journals verify");
    }
    // The solved session ran the solution through exec, as argv and stdin.
    assert!(
        api.state()
            .execs
            .iter()
            .any(|exec| exec.argv == strings(&["/bin/sh", "-s"])
                && String::from_utf8_lossy(&exec.stdin).contains(SOLUTION_MARKER))
    );
}

#[tokio::test]
async fn journal_verify_prints_the_seed_and_check() {
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("journal.jsonl");
    let api = FakeApi::default();
    api.state().sh_handler = Some(fixed_handler(ShAnswer::exit(4)));
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        kiln_harness::journal::JsonlJournal::open(&path).expect("journal"),
        config(),
    );
    let task = SessionTask {
        seed: vec![SeedFile {
            path: "in.txt".to_string(),
            contents: b"x".to_vec(),
        }],
        ..checked_task("exit 4", Duration::from_secs(10))
    };
    harness
        .run(&task, CancellationToken::new())
        .await
        .expect("completes");
    drop(harness);
    let output = cli(&["journal", "verify", path.to_str().expect("utf-8")]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("status: complete (completed)"), "{text}");
    assert!(text.contains("seed: 1 files"), "{text}");
    assert!(text.contains("check: failed (exit 4)"), "{text}");
}

#[tokio::test]
async fn a_checked_seeded_session_against_a_current_kilnd_completes() {
    let api = FakeApi::keyed(true);
    api.state().sh_handler = Some(out_txt_handler());
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new([write_out(), ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    let task = SessionTask {
        seed: seeded_task().seed,
        ..checked_task("test -f /workspace/out.txt", Duration::from_secs(10))
    };
    let outcome = harness
        .run(&task, CancellationToken::new())
        .await
        .expect("completes");
    assert_eq!(outcome.check, Some(CheckOutcome::Passed));
    assert_eq!(
        ended(harness.journal()),
        ("completed".to_string(), "completed".to_string(), true)
    );
    // The expiry covers the whole session, check window included.
    let lifetime = api.state().creates[0]
        .expiry
        .map(|policy| policy.max_lifetime_ms);
    assert_eq!(lifetime, Some(30_000 + 100 + 60_000));
}

// ---------------------------------------------------------------------------
// --max-run-secs and launch order

/// Tasks named `ids`, in that (sorted) order, each with its own session
/// timeout in seconds, or none.
fn timed_tasks(root: &Path, timed: &[(&str, Option<u64>)], base: &HarnessConfig) -> Vec<EvalTask> {
    for (id, secs) in timed {
        let mut json = task_json(id);
        if let Some(secs) = secs {
            json["session_timeout_secs"] = json!(secs);
        }
        make_task(root, id, &json, &[]);
    }
    load_tasks(root, &[], base).expect("loads")
}

#[test]
fn max_run_secs_caps_each_runs_session_timeout() {
    let temp = common::private_tempdir().expect("tempdir");
    let base = config();
    let timed = [("a", Some(1500)), ("b", Some(300)), ("c", None)];
    let tasks = timed_tasks(&temp.path().join("tasks"), &timed, &base);
    assert_eq!(base.session_timeout, Duration::from_secs(30));
    let run_timeouts = |base: &HarnessConfig, cap: Option<u64>| {
        tasks
            .iter()
            .map(|task| task.run_config(base, cap).session_timeout.as_secs())
            .collect::<Vec<_>>()
    };

    assert_eq!(
        run_timeouts(&base, None),
        [1500, 300, 30],
        "unset changes nothing"
    );
    for task in &tasks {
        assert_eq!(
            task.run_config(&base, None).limits(),
            task.config(&base).limits()
        );
    }
    assert_eq!(run_timeouts(&base, Some(600)), [600, 300, 30]);
    for task in &tasks {
        assert_eq!(
            HarnessConfig {
                session_timeout: task.config(&base).session_timeout,
                ..task.run_config(&base, Some(600))
            }
            .limits(),
            task.config(&base).limits(),
            "only the session timeout changes"
        );
    }

    // A task without its own timeout takes the smaller of the base's and
    // the cap.
    let long_base = HarnessConfig {
        session_timeout: Duration::from_secs(1200),
        ..config()
    };
    assert_eq!(run_timeouts(&long_base, Some(600)), [600, 300, 600]);
}

#[test]
fn max_run_secs_out_of_range_or_under_a_check_window_is_refused() {
    let temp = common::private_tempdir().expect("tempdir");
    let base = config();
    let tasks = timed_tasks(
        &temp.path().join("tasks"),
        &[("a", Some(1500)), ("b", None)],
        &base,
    );
    eval::check_run_cap(&tasks, &base, None).expect("unset passes");
    for refused in [0, 6 * 60 * 60 + 1] {
        let error = eval::check_run_cap(&tasks, &base, Some(refused)).expect_err("refused");
        assert!(matches!(error, EvalError::Options(_)), "{error}");
        assert_eq!(
            error.to_string(),
            "--max-run-secs must be between 1 and 21600"
        );
    }
    // Each task's check timeout is 10 s, so the cap must be at least 20.
    let error = eval::check_run_cap(&tasks, &base, Some(19)).expect_err("refused");
    assert_eq!(
        error.to_string(),
        "--max-run-secs 19 is less than twice task a's check timeout of 10 s"
    );
    eval::check_run_cap(&tasks, &base, Some(20)).expect("exactly twice is allowed");
}

#[tokio::test]
async fn run_suite_refuses_a_max_run_secs_under_a_check_window_before_any_output() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = timed_tasks(&temp.path().join("tasks"), &[("a", Some(1500))], &config());
    let out = temp.path().join("out");
    let api = FakeApi::default();
    let error = eval::run_suite(
        &api,
        &tasks,
        &SuiteOptions {
            max_run_secs: Some(19),
            ..options(&out, 1, false)
        },
        || Ok(ScriptedProvider::new([ModelResponse::final_text("done")])),
        &CancellationToken::new(),
        &mut |_| Ok(()),
    )
    .await
    .expect_err("refused");
    assert!(matches!(error, EvalError::Options(_)), "{error}");
    assert!(!out.exists(), "nothing was written");
}

#[test]
fn max_run_secs_reaches_the_manifest_report_and_comparison() {
    let mut capped = manifest(&["a"], 1);
    capped.max_run_secs = Some(600);
    let runs = [result("a", 1, Some("passed"), 2, (10, 1))];
    let base = eval::report(&runs, &manifest(&["a"], 1));
    let new = eval::report(&runs, &capped);
    assert_eq!((base.max_run_secs, new.max_run_secs), (None, Some(600)));

    let json = serde_json::to_value(&capped).expect("manifest JSON");
    assert_eq!(json["max_run_secs"], 600);
    let back: Manifest = serde_json::from_value(json).expect("round-trips");
    assert_eq!(back, capped);
    let json = serde_json::to_value(&new).expect("report JSON");
    assert_eq!(json["max_run_secs"], 600);
    let back: Report = serde_json::from_value(json).expect("round-trips");
    assert_eq!(back, new);

    // Unset, and in files written before the field existed: no key.
    let unset = serde_json::to_value(manifest(&["a"], 1)).expect("manifest JSON");
    assert!(unset.get("max_run_secs").is_none(), "{unset}");
    let older: Manifest = serde_json::from_value(unset).expect("an older manifest parses");
    assert_eq!(older.max_run_secs, None);
    let unset = serde_json::to_value(&base).expect("report JSON");
    assert!(unset.get("max_run_secs").is_none(), "{unset}");
    let temp = common::private_tempdir().expect("tempdir");
    write(
        &temp.path().join("report.json"),
        unset.to_string().as_bytes(),
    );
    assert_eq!(
        eval::read_report(temp.path())
            .expect("an older report reads")
            .max_run_secs,
        None
    );

    let markdown = eval::report_markdown(&new);
    assert!(
        markdown.contains("- Runs at a time: 1\n- Longest run allowed: 600 s\n"),
        "{markdown}"
    );
    assert!(
        !eval::report_markdown(&base).contains("Longest run"),
        "unset says nothing"
    );

    let differing = eval::compare(&base, &new);
    assert!(
        differing.ends_with(
            ").\n\nLongest run allowed differs (none → 600); pass rates and wall times are not \
             directly comparable.\n"
        ),
        "{differing}"
    );
    let mut shorter = new.clone();
    shorter.max_run_secs = Some(300);
    assert!(
        eval::compare(&new, &shorter).contains("Longest run allowed differs (600 → 300)"),
        "both set"
    );
    for same in [eval::compare(&base, &base), eval::compare(&new, &new)] {
        assert!(!same.contains("Longest run"), "{same}");
    }
}

#[test]
fn effort_and_output_limit_reach_the_manifest_report_and_comparison() {
    let mut low = manifest(&["a"], 1);
    low.effort = Some("low".to_string());
    low.max_output_tokens = Some(64_000);
    let runs = [result("a", 1, Some("passed"), 2, (10, 1))];
    let older = eval::report(&runs, &manifest(&["a"], 1));
    let new = eval::report(&runs, &low);
    assert_eq!(
        (new.effort.as_deref(), new.max_output_tokens),
        (Some("low"), Some(64_000))
    );

    let json = serde_json::to_value(&low).expect("manifest JSON");
    assert_eq!(
        (&json["effort"], &json["max_output_tokens"]),
        (&json!("low"), &json!(64_000))
    );
    let back: Manifest = serde_json::from_value(json).expect("round-trips");
    assert_eq!(back, low);
    let json = serde_json::to_value(&new).expect("report JSON");
    assert_eq!(
        (&json["effort"], &json["max_output_tokens"]),
        (&json!("low"), &json!(64_000))
    );
    let back: Report = serde_json::from_value(json).expect("round-trips");
    assert_eq!(back, new);

    // Unset, and in files written before the fields existed: no keys.
    let unset = serde_json::to_value(&older).expect("report JSON");
    for key in ["effort", "max_output_tokens"] {
        assert!(unset.get(key).is_none(), "{unset}");
    }
    let parsed: Report = serde_json::from_value(unset).expect("an older report parses");
    assert_eq!((parsed.effort, parsed.max_output_tokens), (None, None));

    let markdown = eval::report_markdown(&new);
    assert!(
        markdown.contains("- Effort: low\n- Output limit per response: 64000 tokens\n"),
        "{markdown}"
    );
    let markdown = eval::report_markdown(&older);
    assert!(
        !markdown.contains("Effort") && !markdown.contains("Output limit"),
        "{markdown}"
    );

    let mut high = new.clone();
    high.effort = Some("high".to_string());
    high.max_output_tokens = Some(16_384);
    let differing = eval::compare(&high, &new);
    assert!(
        differing.ends_with(
            ").\n\nEffort differs (high → low); pass rates, tokens and wall times are not \
             directly comparable.\n\nOutput limit per response differs (16384 → 64000 tokens); \
             pass rates and tokens are not directly comparable.\n"
        ),
        "{differing}"
    );
    // An older report recorded neither: unknown is not a difference.
    for same in [
        eval::compare(&new, &new),
        eval::compare(&older, &new),
        eval::compare(&new, &older),
    ] {
        assert!(
            !same.contains("Effort") && !same.contains("Output limit"),
            "{same}"
        );
    }
    // A hostile effort string stays inside its line.
    let mut odd = new.clone();
    odd.effort = Some("x|y\nz".to_string());
    assert!(eval::report_markdown(&odd).contains("- Effort: x\\|y z\n"));
}

#[tokio::test]
async fn runs_start_longest_session_timeout_first_and_report_in_task_order() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = timed_tasks(
        &temp.path().join("tasks"),
        &[("a", Some(300)), ("b", Some(1500)), ("c", Some(900))],
        &config(),
    );
    let out = temp.path().join("out");
    let api = FakeApi::default();
    api.state().sh_handler = Some(out_txt_handler());
    let mut progress = Vec::new();
    let summary = eval::run_suite(
        &api,
        &tasks,
        &options(&out, 2, false),
        || Ok(ScriptedProvider::new([ModelResponse::final_text("done")])),
        &CancellationToken::new(),
        &mut |result| {
            progress.push((result.task.clone(), result.trial));
            Ok(())
        },
    )
    .await
    .expect("the suite runs");

    // One at a time, so runs end in the order they started.
    let started = [("b", 1), ("b", 2), ("c", 1), ("c", 2), ("a", 1), ("a", 2)]
        .map(|(task, trial)| (task.to_string(), trial));
    assert_eq!(progress, started);
    let lines = read_lines(&out.join("results.jsonl"))
        .into_iter()
        .map(|result| (result.task, result.trial))
        .collect::<Vec<_>>();
    assert_eq!(lines, started);

    let canonical = [("a", 1), ("a", 2), ("b", 1), ("b", 2), ("c", 1), ("c", 2)]
        .map(|(task, trial)| (task.to_string(), trial));
    let results = summary
        .results
        .iter()
        .map(|result| (result.task.clone(), result.trial))
        .collect::<Vec<_>>();
    assert_eq!(results, canonical);
    let report_tasks = summary
        .report
        .tasks
        .iter()
        .map(|task| task.task.as_str())
        .collect::<Vec<_>>();
    assert_eq!(report_tasks, ["a", "b", "c"]);
    assert_eq!(read_manifest(&out).tasks, ["a", "b", "c"]);
    assert_eq!(read_manifest(&out).max_run_secs, None);
    // The suite records the output limit it ran with, and the effort it
    // was given (none for this provider).
    assert_eq!(
        read_manifest(&out).max_output_tokens,
        Some(config().max_output_tokens_per_request)
    );
    assert_eq!(read_manifest(&out).effort, None);
    let markdown = std::fs::read_to_string(out.join("report.md")).expect("markdown");
    let rows = markdown
        .lines()
        .filter(|line| line.starts_with("| ") && !line.starts_with("| Task"))
        .filter_map(|line| line.split(' ').nth(1))
        .filter(|task| ["a", "b", "c"].contains(task))
        .collect::<Vec<_>>();
    assert_eq!(rows, ["a", "b", "c"]);
}

#[tokio::test]
async fn a_cap_that_ties_every_task_still_starts_the_longest_budgets_first() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = timed_tasks(
        &temp.path().join("tasks"),
        &[("a", Some(300)), ("b", Some(1500)), ("c", Some(900))],
        &config(),
    );
    let out = temp.path().join("out");
    let api = FakeApi::default();
    api.state().sh_handler = Some(out_txt_handler());
    let mut progress = Vec::new();
    // 200 s caps every task, so all three run under the same limit.
    eval::run_suite(
        &api,
        &tasks,
        &SuiteOptions {
            max_run_secs: Some(200),
            ..options(&out, 1, false)
        },
        || Ok(ScriptedProvider::new([ModelResponse::final_text("done")])),
        &CancellationToken::new(),
        &mut |result| {
            progress.push(result.task.clone());
            Ok(())
        },
    )
    .await
    .expect("the suite runs");
    assert_eq!(progress, ["b", "c", "a"]);
    assert_eq!(read_manifest(&out).max_run_secs, Some(200));
}

#[tokio::test(start_paused = true)]
async fn a_capped_run_ends_at_the_deadline_and_fails() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = timed_tasks(&temp.path().join("tasks"), &[("a", Some(1500))], &config());
    let out = temp.path().join("out");
    let api = FakeApi::default();
    api.state().sh_handler = Some(out_txt_handler());
    let seen = Seen::default();
    let started = Instant::now();
    let summary = eval::run_suite(
        &api,
        &tasks,
        &SuiteOptions {
            max_run_secs: Some(20),
            ..options(&out, 1, false)
        },
        || {
            // Slower than the cap, well inside the task's own 1500 s.
            let mut slow = Observing::new(&api, &seen, vec![ModelResponse::final_text("done")]);
            slow.delay = Duration::from_secs(60);
            Ok(slow)
        },
        &CancellationToken::new(),
        &mut |_| Ok(()),
    )
    .await
    .expect("the suite runs");

    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
    let [result] = &summary.results[..] else {
        panic!("one run: {:?}", summary.results);
    };
    assert_eq!(result.outcome, "deadline", "{result:?}");
    assert!(!result.passed());
    assert_eq!(summary.report.overall.passed, 0);
    assert_eq!(read_manifest(&out).max_run_secs, Some(20));
}

#[test]
fn eval_run_refuses_an_out_of_range_max_run_secs_before_contacting_anything() {
    let temp = common::private_tempdir().expect("tempdir");
    let tasks = Path::new(env!("CARGO_MANIFEST_DIR")).join("evals/tasks");
    let out = temp.path().join("out");
    for max_run_secs in ["0", "21601"] {
        let output = cli(&[
            "--socket",
            temp.path().join("no.sock").to_str().expect("utf-8"),
            "eval",
            "run",
            "--provider",
            "openai-compat",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "m",
            "--tasks",
            tasks.to_str().expect("utf-8"),
            "--max-run-secs",
            max_run_secs,
            "--out",
            out.to_str().expect("utf-8"),
        ]);
        assert!(!output.status.success(), "{max_run_secs}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--max-run-secs must be between 1 and 21600"),
            "{stderr}"
        );
        assert!(!stderr.contains("no.sock"), "{stderr}");
        assert!(!out.exists());
    }
}
