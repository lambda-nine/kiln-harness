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
use kiln_harness::api::ExecutionState;
use kiln_harness::audit::{AuditError, AuditEvent, AuditSink, MemoryAudit};
use kiln_harness::engine::{
    CheckOutcome, Harness, HarnessConfig, HarnessError, SeedFile, SessionTask, TaskCheck,
};
use kiln_harness::eval::{
    self, EvalError, EvalTask, Manifest, Report, RunResult, Stat, SuiteOptions, load_tasks,
};
use kiln_harness::journal::{
    JournalEvent, LoadedJournal, MemoryJournal, RecordedSeedFile, read_journal,
};
use kiln_harness::provider::{
    ModelRequest, ModelResponse, Provider, ProviderError, ProviderInfo, ScriptedProvider, ToolCall,
};
use kiln_harness::replay::ReplayProvider;
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
    // recovery, the journal is byte-identical to before.
    let mut harness = Harness::new(
        FakeApi::default(),
        ScriptedProvider::demo(),
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
    let events = harness
        .journal()
        .records
        .iter()
        .map(|record| {
            let mut event = record.event.clone();
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
        &mut |result| progress.push(result.clone()),
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
    let markdown = std::fs::read_to_string(out.join("report.md")).expect("markdown");
    assert!(
        markdown.contains("| alpha | 2 | 2 | 100.0% |"),
        "{markdown}"
    );
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
        &mut |_| {},
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
        &mut |_| {},
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
        &mut |_| {},
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
        &mut |_| {},
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
        &mut |_| {},
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
        &mut |_| {},
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
        &mut |_| cancel.cancel(),
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
