//! A lowered or raised tool result limit (`--max-result-bytes`): every
//! tool's result stays within it at the CLI's 16 KiB minimum, and a session
//! run with it records it and replays under it.
mod common;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::process::Command;
use std::sync::Arc;

use common::{FakeApi, ShAnswer, short_lifecycle_config};
use kiln_harness::audit::MemoryAudit;
use kiln_harness::engine::{Harness, HarnessConfig, SessionTask};
use kiln_harness::journal::{
    JournalEvent, JournalRecord, JsonlJournal, MemoryJournal, read_journal,
};
use kiln_harness::provider::{ModelResponse, ScriptedProvider, ToolCall};
use kiln_harness::providers::default_system_prompt_for;
use kiln_harness::replay::ReplayProvider;
use kiln_harness::tools::ToolSet;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const MIN: usize = 16 << 10;
const MAX: usize = 512 << 10;

fn config(tool_set: ToolSet, max_result_bytes: usize) -> HarnessConfig {
    let mut config = HarnessConfig {
        max_steps: 12,
        max_tool_calls: 64,
        tool_set,
        ..short_lifecycle_config()
    };
    config.tool_limits.max_result_bytes = max_result_bytes;
    config
}

/// 64 KiB of JSON-like text, the per-file limit: quotes, tabs, newlines,
/// a backslash and a multibyte character on every line, so its encoded
/// form is well over its raw size.
fn escaped_text() -> Vec<u8> {
    let line = "\t{\"key\": \"value \\\\ é\", \"n\": 1}\n";
    let mut text = line.repeat((64 << 10) / line.len() + 1).into_bytes();
    text.truncate(64 << 10);
    text
}

/// Many matches for `search`, each line under its text cut.
fn many_matches() -> Vec<u8> {
    let mut text = String::new();
    for index in 0..4000 {
        let _ = writeln!(text, "match \"{index}\"\tline");
    }
    text.into_bytes()
}

fn api() -> FakeApi {
    let api = FakeApi::default();
    {
        let mut state = api.state();
        state
            .files
            .insert("/workspace/data.json".to_string(), escaped_text());
        state
            .files
            .insert("/workspace/matches.txt".to_string(), many_matches());
        // Long output on both streams, as a build log has.
        state.sh_handler = Some(Arc::new(|_: &[u8], _: &mut BTreeMap<String, Vec<u8>>| {
            ShAnswer {
                stdout: "compiling module 42 of 9000\n".repeat(50_000).into_bytes(),
                stderr: "warning: unused variable `x`\n".repeat(50_000).into_bytes(),
                ..ShAnswer::exit(0)
            }
        }));
    }
    api
}

fn call(id: &str, tool: &str, arguments: Value) -> ToolCall {
    ToolCall::new(id, tool, arguments)
}

fn script(tool_set: ToolSet) -> Vec<ModelResponse> {
    let mut turns = vec![
        ModelResponse::tool_use([
            call("read", "read_file", json!({"path": "data.json"})),
            call(
                "read_small",
                "read_file",
                json!({"path": "data.json", "max_bytes": 100}),
            ),
            call(
                "exec",
                "exec",
                json!({"argv": ["/bin/sh", "-s"], "stdin": "print"}),
            ),
            call("list", "list_dir", json!({"path": "."})),
            call("diff", "workspace_diff", json!({})),
        ]),
        ModelResponse::final_text("done"),
    ];
    if tool_set != ToolSet::Base {
        turns.insert(
            1,
            ModelResponse::tool_use([
                call(
                    "lines",
                    "read_file",
                    json!({"path": "data.json", "start_line": 1, "line_count": 5000}),
                ),
                call(
                    "search",
                    "search",
                    json!({"pattern": "match", "max_results": 200}),
                ),
            ]),
        );
    }
    turns
}

async fn session(api: &FakeApi, config: HarnessConfig) -> Vec<JournalRecord> {
    let task = SessionTask::new(
        default_system_prompt_for(&config.tool_limits, config.tool_set),
        "Look at the workspace.",
    );
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new(script(config.tool_set)),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let outcome = harness
        .run(&task, CancellationToken::new())
        .await
        .expect("the session completes");
    assert_eq!(outcome.final_answer, "done");
    harness.journal().records.clone()
}

fn results(records: &[JournalRecord]) -> BTreeMap<String, Value> {
    records
        .iter()
        .filter_map(|record| match &record.event {
            JournalEvent::ToolExecuted { call, result, .. } => {
                Some((call.call_id.clone(), result.clone()))
            }
            _ => None,
        })
        .collect()
}

fn encoded(result: &Value) -> usize {
    serde_json::to_vec(result).expect("encode").len()
}

#[tokio::test]
async fn every_result_fits_the_lowest_limit_and_reports_what_it_cut() {
    let file = escaped_text();
    for tool_set in [ToolSet::Base, ToolSet::CodingV2] {
        let results = results(&session(&api(), config(tool_set, MIN)).await);
        for (id, result) in &results {
            assert!(
                encoded(result) <= MIN,
                "{tool_set:?} {id}: {}",
                encoded(result)
            );
            assert_eq!(result["ok"], true, "{tool_set:?} {id}: {result}");
        }

        // A whole-file read returns the start that fits, as truncated.
        let read = &results["read"];
        let returned = usize::try_from(read["returned_bytes"].as_u64().expect("returned_bytes"))
            .expect("fits");
        assert!(returned > MIN / 2 && returned < MIN, "{returned}");
        assert_eq!(
            read["content"].as_str().map(str::as_bytes),
            Some(&file[..returned])
        );
        assert_eq!(
            (&read["truncated"], &read["utf8"]),
            (&json!(true), &json!(true))
        );
        assert_eq!(read["file_bytes"], file.len());
        // Within a few bytes of the bound: the next character did not fit.
        assert!(encoded(read) > MIN - 1024, "{}", encoded(read));
        // A read under the bound is as it was.
        assert_eq!(
            results["read_small"],
            json!({"ok": true, "content": String::from_utf8_lossy(&file[..100]), "utf8": true,
                   "file_bytes": file.len(), "returned_bytes": 100, "truncated": true})
        );

        let exec = &results["exec"];
        assert_eq!(
            (
                &exec["stdout_model_truncated"],
                &exec["stderr_model_truncated"]
            ),
            (&json!(true), &json!(true))
        );
        assert_eq!(exec["stdout"].as_str().map(str::len), Some(MIN / 3));

        if tool_set != ToolSet::Base {
            let lines = &results["lines"];
            assert_eq!(lines["truncated"], true, "{lines}");
            assert!(lines["returned_lines"].as_u64() > Some(100), "{lines}");
            let search = &results["search"];
            assert_eq!(search["truncated"], true, "{search}");
            assert!(search["returned"].as_u64() > Some(100), "{search}");
        }
    }
}

/// At the highest limit, and at the default, a whole 64 KiB read is
/// returned whole, exactly as before the limit could be set.
#[tokio::test]
async fn a_raised_limit_returns_the_whole_file() {
    let file = escaped_text();
    for max_result_bytes in [128 << 10, MAX] {
        let results = results(&session(&api(), config(ToolSet::CodingV2, max_result_bytes)).await);
        let read = &results["read"];
        assert_eq!(read["returned_bytes"], file.len(), "{max_result_bytes}");
        assert_eq!(read["truncated"], false);
        assert_eq!(
            read["content"].as_str().map(str::as_bytes),
            Some(file.as_slice())
        );
        let search = &results["search"];
        assert_eq!(search["returned"], 200, "{max_result_bytes}");
        for (id, result) in &results {
            assert!(encoded(result) <= max_result_bytes, "{id}");
            assert_eq!(result["ok"], true, "{id}: {result}");
        }
    }
}

#[tokio::test]
async fn a_lowered_limit_is_journaled_and_replays_under_it() {
    let temp = common::private_tempdir().expect("temp dir");
    let path = temp.path().join("lowered.jsonl");
    let config = config(ToolSet::CodingV2, 32 << 10);
    let task = SessionTask::new(
        default_system_prompt_for(&config.tool_limits, config.tool_set),
        "Look at the workspace.",
    );
    assert!(task.system.contains("at most 32 KiB;"), "{}", task.system);
    let mut harness = Harness::new(
        api(),
        ScriptedProvider::new(script(ToolSet::CodingV2)),
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        config,
    );
    harness
        .run(&task, CancellationToken::new())
        .await
        .expect("the session completes");
    drop(harness);

    let journal = read_journal(&path).expect("the journal verifies");
    let recorded = journal
        .records
        .iter()
        .find_map(|record| match &record.event {
            JournalEvent::SessionStarted { limits, .. } => Some(*limits),
            _ => None,
        })
        .expect("session_started");
    assert_eq!(recorded.tool_limits.max_result_bytes, 32 << 10);
    let original = results(&journal.records);
    assert_eq!(original["read"]["truncated"], true);
    assert!(encoded(&original["read"]) <= 32 << 10);

    // The replay restores the recorded limit, whatever its base config
    // says, and reproduces every result under it.
    let provider = ReplayProvider::from_journal(&journal).expect("replayable");
    let replay_config = provider.replay_config(short_lifecycle_config());
    assert_eq!(replay_config.tool_limits.max_result_bytes, 32 << 10);
    let mut replay = Harness::new(
        api(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        replay_config,
    );
    let outcome = replay
        .replay(CancellationToken::new())
        .await
        .expect("the replay completes");
    assert_eq!(outcome.final_answer, "done");
    assert_eq!(replay.provider().diverged_at(), None);
    assert!(replay.provider().exhausted());
    assert_eq!(results(&replay.journal().records), original);

    let output = Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
        .args(["journal", "verify"])
        .arg(&path)
        .output()
        .expect("run journal verify");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
}

/// A workspace whose seed files are all replaced: `count` removed and
/// `count` added paths, each `stem` plus its number.
fn replaced_workspace(count: usize, stem: &str) -> (FakeApi, Vec<String>, Vec<String>) {
    let name = |side: &str, index: usize| format!("{side}{stem}{index:04}");
    let removed: Vec<String> = (0..count).map(|index| name("old", index)).collect();
    let added: Vec<String> = (0..count).map(|index| name("new", index)).collect();
    let api = FakeApi::default();
    {
        let mut state = api.state();
        for path in &removed {
            state
                .files
                .insert(format!("/workspace/{path}"), b"x".to_vec());
        }
        let fresh = added.clone();
        state.sh_handler = Some(Arc::new(
            move |_: &[u8], files: &mut BTreeMap<String, Vec<u8>>| {
                files.clear();
                for path in &fresh {
                    files.insert(format!("/workspace/{path}"), b"y".to_vec());
                }
                ShAnswer::exit(0)
            },
        ));
    }
    (api, added, removed)
}

async fn diff_after_replacing(api: &FakeApi, max_result_bytes: usize) -> Value {
    let config = config(ToolSet::Base, max_result_bytes);
    let task = SessionTask::new(
        default_system_prompt_for(&config.tool_limits, config.tool_set),
        "Replace the files.",
    );
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new(vec![
            ModelResponse::tool_use([call(
                "replace",
                "exec",
                json!({"argv": ["/bin/sh", "-s"], "stdin": "replace"}),
            )]),
            ModelResponse::tool_use([call("diff", "workspace_diff", json!({}))]),
            ModelResponse::final_text("done"),
        ]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    harness
        .run(&task, CancellationToken::new())
        .await
        .expect("the session completes");
    results(&harness.journal().records)["diff"].clone()
}

fn strings(value: &Value) -> Vec<String> {
    serde_json::from_value(value.clone()).expect("a list of paths")
}

/// Lists over the bound are cut to fit, each a start of its sorted list,
/// with the totals; the budget is shared, so both lists show.
fn assert_cut_to_fit(diff: &Value, added: &[String], removed: &[String], bound: usize) {
    assert!(encoded(diff) <= bound, "{}", encoded(diff));
    assert_eq!(diff["ok"], true);
    assert_eq!(diff["truncated"], true);
    assert_eq!(diff["added_total"], added.len());
    assert_eq!(diff["removed_total"], removed.len());
    let (kept_added, kept_removed) = (strings(&diff["added"]), strings(&diff["removed"]));
    assert!(!kept_added.is_empty() && kept_added.len() < added.len());
    assert!(!kept_removed.is_empty() && kept_removed.len() < removed.len());
    assert_eq!(kept_added, added[..kept_added.len()]);
    assert_eq!(kept_removed, removed[..kept_removed.len()]);
    // Within a path or two of the bound, less the reserve for the rest.
    assert!(encoded(diff) > bound - 1024 - 2 * 600, "{}", encoded(diff));
}

#[tokio::test]
async fn a_workspace_diff_over_the_limit_is_cut_to_fit_and_says_so() {
    // 300 paths a side, about 24 KiB of JSON together: over the minimum.
    let (api, added, removed) = replaced_workspace(300, "-generated-module-source-file-");
    let diff = diff_after_replacing(&api, MIN).await;
    assert_cut_to_fit(&diff, &added, &removed, MIN);

    // The same diff under the default is whole and exactly as before.
    let (api, added, removed) = replaced_workspace(300, "-generated-module-source-file-");
    let diff = diff_after_replacing(&api, 128 << 10).await;
    assert_eq!(
        diff,
        json!({"ok": true, "added": added, "removed": removed})
    );
}

#[tokio::test]
async fn a_workspace_diff_of_escaped_paths_is_cut_to_fit_at_the_default() {
    // 360 paths a side within the manifest's 64 KiB each, mostly quotes,
    // so the two lists encode to over the default 128 KiB.
    let stem = format!("-{}-", "\"".repeat(140));
    let (api, added, removed) = replaced_workspace(360, &stem);
    let diff = diff_after_replacing(&api, 128 << 10).await;
    assert_cut_to_fit(&diff, &added, &removed, 128 << 10);
}

/// Output on each stream that doubles when escaped: quotes and tabs.
fn escaping(byte_count: usize) -> Vec<u8> {
    "\"\t".repeat(byte_count / 2).into_bytes()
}

fn exec_api(stdout: Vec<u8>, stderr: Vec<u8>) -> FakeApi {
    let api = FakeApi::default();
    api.state().sh_handler = Some(Arc::new(
        move |_: &[u8], _: &mut BTreeMap<String, Vec<u8>>| ShAnswer {
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            ..ShAnswer::exit(0)
        },
    ));
    api
}

fn exec_script() -> Vec<ModelResponse> {
    vec![
        ModelResponse::tool_use([call(
            "exec",
            "exec",
            json!({"argv": ["/bin/sh", "-s"], "stdin": "print"}),
        )]),
        ModelResponse::final_text("done"),
    ]
}

async fn exec_result(api: &FakeApi, max_result_bytes: usize) -> Value {
    let config = config(ToolSet::Base, max_result_bytes);
    let task = SessionTask::new(
        default_system_prompt_for(&config.tool_limits, config.tool_set),
        "Print it.",
    );
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new(exec_script()),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    harness
        .run(&task, CancellationToken::new())
        .await
        .expect("the session completes");
    results(&harness.journal().records)["exec"].clone()
}

/// Output that escapes past the bound keeps the start of each stream that
/// fits, both shown, each marked cut.
fn assert_exec_cut(exec: &Value, stdout: &[u8], stderr: &[u8], bound: usize) {
    assert!(encoded(exec) <= bound, "{}", encoded(exec));
    assert!(encoded(exec) > bound - 16, "{}", encoded(exec));
    assert_eq!(exec["ok"], true, "{exec}");
    for (name, raw) in [("stdout", stdout), ("stderr", stderr)] {
        let text = exec[name].as_str().expect("text");
        assert!(text.len() > bound / 8, "{name}: {}", text.len());
        assert_eq!(text.as_bytes(), &raw[..text.len()], "{name}");
        assert_eq!(exec[format!("{name}_model_truncated")], true, "{name}");
        assert_eq!(exec[format!("{name}_truncated")], true, "{name}");
        assert_eq!(exec[format!("{name}_observed_bytes")], raw.len(), "{name}");
    }
}

#[tokio::test]
async fn exec_output_that_escapes_past_the_limit_is_cut_to_fit() {
    let (stdout, stderr) = (escaping(200_000), escaping(150_000));
    for bound in [MIN, 128 << 10, MAX] {
        let exec = exec_result(&exec_api(stdout.clone(), stderr.clone()), bound).await;
        assert_exec_cut(&exec, &stdout, &stderr, bound);
        // The two streams share the room evenly when both are long.
        let (out, err) = (
            exec["stdout"].as_str().map(str::len),
            exec["stderr"].as_str().map(str::len),
        );
        assert!(
            out.zip(err)
                .is_some_and(|(out, err)| out.abs_diff(err) <= 2),
            "{out:?} {err:?}"
        );
    }
}

#[tokio::test]
async fn one_escaping_stream_takes_the_room_the_other_leaves() {
    // A third of the bound in control bytes, six bytes each escaped
    // (`\u0001`), is twice the bound on its own.
    let stdout = vec![1_u8; 200_000];
    let bound = 128 << 10;
    let exec = exec_result(&exec_api(stdout.clone(), b"one line\n".to_vec()), bound).await;
    assert!(
        encoded(&exec) <= bound && encoded(&exec) > bound - 16,
        "{}",
        encoded(&exec)
    );
    assert_eq!(exec["stderr"], "one line\n");
    assert_eq!(exec["stderr_model_truncated"], false);
    let text = exec["stdout"].as_str().expect("text");
    assert_eq!(text.as_bytes(), &stdout[..text.len()]);
    // Nearly all of the bound, at six bytes a byte.
    assert!(text.len() > (bound - 1024) / 6, "{}", text.len());
    assert_eq!(exec["stdout_model_truncated"], true);
}

#[tokio::test]
async fn exec_output_that_fits_is_as_it_was() {
    // Escaping that still fits, and plain output cut only by its third.
    for (stdout, stderr) in [
        (escaping(20_000), escaping(20_000)),
        (b"x".repeat(100_000), b"y".repeat(100_000)),
    ] {
        let exec = exec_result(&exec_api(stdout.clone(), stderr.clone()), 128 << 10).await;
        let third = (128 << 10) / 3;
        for (name, raw) in [("stdout", &stdout), ("stderr", &stderr)] {
            let end = raw.len().min(third);
            assert_eq!(
                exec[name].as_str().map(str::as_bytes),
                Some(&raw[..end]),
                "{name}"
            );
            assert_eq!(
                exec[format!("{name}_model_truncated")],
                raw.len() > third,
                "{name}"
            );
        }
    }
}

#[tokio::test]
async fn a_cut_exec_result_is_journaled_and_replays_the_same() {
    let temp = common::private_tempdir().expect("temp dir");
    let path = temp.path().join("cut-exec.jsonl");
    let (stdout, stderr) = (escaping(100_000), escaping(100_000));
    let config = config(ToolSet::Base, 32 << 10);
    let task = SessionTask::new(
        default_system_prompt_for(&config.tool_limits, config.tool_set),
        "Print it.",
    );
    let mut harness = Harness::new(
        exec_api(stdout.clone(), stderr.clone()),
        ScriptedProvider::new(exec_script()),
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        config,
    );
    harness
        .run(&task, CancellationToken::new())
        .await
        .expect("the session completes");
    drop(harness);

    let journal = read_journal(&path).expect("the journal verifies");
    let original = results(&journal.records);
    assert_exec_cut(&original["exec"], &stdout, &stderr, 32 << 10);
    let provider = ReplayProvider::from_journal(&journal).expect("replayable");
    let replay_config = provider.replay_config(short_lifecycle_config());
    let mut replay = Harness::new(
        exec_api(stdout, stderr),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        replay_config,
    );
    replay
        .replay(CancellationToken::new())
        .await
        .expect("the replay completes");
    assert_eq!(replay.provider().diverged_at(), None);
    assert_eq!(results(&replay.journal().records), original);
}

/// Run each of `turns` as one model response, then answer "done"; return
/// each call's journaled result by call ID.
async fn turn_results(
    api: &FakeApi,
    config: HarnessConfig,
    turns: Vec<Vec<ToolCall>>,
) -> BTreeMap<String, Value> {
    let task = SessionTask::new(
        default_system_prompt_for(&config.tool_limits, config.tool_set),
        "Look at the workspace.",
    );
    let mut script: Vec<_> = turns.into_iter().map(ModelResponse::tool_use).collect();
    script.push(ModelResponse::final_text("done"));
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new(script),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    harness
        .run(&task, CancellationToken::new())
        .await
        .expect("the session completes");
    results(&harness.journal().records)
}

/// A name of `len` characters, all `"` or `\` (each two bytes in JSON),
/// spelling `index` in binary so every name is distinct. Neither is a
/// control character, so the baseline manifest accepts it.
fn escaping_name(index: usize, len: usize) -> String {
    (0..len)
        .map(|bit| {
            if bit < usize::BITS as usize && (index >> bit) & 1 == 1 {
                '\\'
            } else {
                '"'
            }
        })
        .collect()
}

/// A workspace in which the model's first `exec` creates `names`, after
/// the baseline, as a build or a hostile repository's script could.
fn naming_api(names: Vec<String>) -> FakeApi {
    let api = FakeApi::default();
    api.state().sh_handler = Some(Arc::new(
        move |_: &[u8], files: &mut BTreeMap<String, Vec<u8>>| {
            for name in &names {
                files.insert(format!("/workspace/{name}"), b"x".to_vec());
            }
            ShAnswer::exit(0)
        },
    ));
    api
}

fn make_then(calls: Vec<ToolCall>) -> Vec<Vec<ToolCall>> {
    vec![
        vec![call(
            "make",
            "exec",
            json!({"argv": ["/bin/sh", "-s"], "stdin": "make files"}),
        )],
        calls,
    ]
}

/// What `ls -1Ap` prints for the fake's workspace.
fn fake_listing(api: &FakeApi) -> String {
    let state = api.state();
    let names: Vec<_> = state
        .files
        .keys()
        .filter_map(|path| path.strip_prefix("/workspace/"))
        .collect();
    names.join("\n") + "\n"
}

/// A listing over the bound keeps the start of `ls` output that fits,
/// marked truncated.
fn assert_listing_cut(list: &Value, listing: &str, bound: usize) {
    assert!(encoded(list) <= bound, "{}", encoded(list));
    assert!(encoded(list) > bound - 16, "{}", encoded(list));
    assert_eq!(list["ok"], true, "{list}");
    assert_eq!(list["listing_truncated"], true);
    let text = list["listing"].as_str().expect("listing");
    assert!(text.len() > bound / 8, "{}", text.len());
    assert!(listing.starts_with(text));
}

#[tokio::test]
async fn a_list_dir_of_escaping_names_is_cut_to_fit_at_the_lowest_limit() {
    // 80 names of 127 quotes and backslashes: about 10 KiB raw, so half the
    // 16 KiB bound in raw bytes encodes to more than the whole bound.
    let names = (0..80).map(|index| escaping_name(index, 127)).collect();
    let api = naming_api(names);
    let results = turn_results(
        &api,
        config(ToolSet::Base, MIN),
        make_then(vec![
            call("list", "list_dir", json!({"path": "."})),
            call("diff", "workspace_diff", json!({})),
        ]),
    )
    .await;
    assert_eq!(results["make"]["ok"], true, "{}", results["make"]);
    assert_listing_cut(&results["list"], &fake_listing(&api), MIN);
    // workspace_diff on the same names is cut to fit too.
    let diff = &results["diff"];
    assert_eq!(
        (&diff["ok"], &diff["truncated"]),
        (&json!(true), &json!(true))
    );
}

#[tokio::test]
async fn a_list_dir_of_escaping_names_is_cut_to_fit_at_the_default() {
    let bound = 128 << 10;
    // Quotes and backslashes, which the baseline accepts: a listing over
    // half the bound whose raw half encodes to about the whole bound. And
    // control characters a sandbox program can create after the baseline
    // (`ls` into a pipe prints them raw), six bytes each in JSON.
    let quoted = (0..600).map(|index| escaping_name(index, 127)).collect();
    let control = (0..300)
        .map(|index| format!("{index:03}{}", "\u{1}".repeat(240)))
        .collect();
    for names in [quoted, control] {
        let api = naming_api(names);
        let results = turn_results(
            &api,
            config(ToolSet::CodingV2, bound),
            make_then(vec![call("list", "list_dir", json!({"path": "."}))]),
        )
        .await;
        assert_eq!(results["make"]["ok"], true, "{}", results["make"]);
        assert_listing_cut(&results["list"], &fake_listing(&api), bound);
    }
}

#[tokio::test]
async fn a_list_dir_that_fits_is_as_it_was() {
    // Escaping names that still fit, and plain names cut only by their half.
    let escaping = (0..40).map(|index| escaping_name(index, 127)).collect();
    let plain = (0..1000).map(|index| format!("{index:0>100}")).collect();
    for names in [escaping, plain] {
        let api = naming_api(names);
        let results = turn_results(
            &api,
            config(ToolSet::Base, MIN),
            make_then(vec![call("list", "list_dir", json!({"path": "."}))]),
        )
        .await;
        let listing = fake_listing(&api);
        let end = listing.len().min(MIN / 2);
        let list = &results["list"];
        assert_eq!(list["listing"], listing[..end], "{list}");
        assert_eq!(list["listing_truncated"], listing.len() > end);
    }
}
