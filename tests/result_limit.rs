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
        // Long output on both streams, as a build log has. Output that
        // escapes to more than half again its size can pass any limit,
        // the default included, and is refused whole as before.
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
