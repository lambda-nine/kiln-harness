//! The opt-in `coding_v1` tool set: ranged `read_file`, `edit_file` and
//! `search` against the fake kilnd, its journal record and replay, and the
//! base set's byte-for-byte compatibility.
mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use common::{FakeApi, ShAnswer, short_lifecycle_config};
use kiln_harness::api::CapturedOutput;
use kiln_harness::audit::{AuditEvent, MemoryAudit};
use kiln_harness::engine::{Harness, HarnessConfig, HarnessError, SessionTask};
use kiln_harness::eval::{self, Manifest, Report, RunResult, SuiteOptions};
use kiln_harness::journal::{
    JournalError, JournalEvent, JournalRecord, LoadedJournal, MemoryJournal, read_journal,
};
use kiln_harness::provider::{
    ModelRequest, ModelResponse, Provider, ProviderError, ProviderInfo, ScriptedProvider, ToolCall,
};
use kiln_harness::providers::{default_system_prompt, default_system_prompt_for};
use kiln_harness::replay::ReplayProvider;
use kiln_harness::tools::{ToolLimits, ToolRegistry, ToolSet};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// A provider that keeps what each request carried.
struct Recording<P> {
    inner: P,
    requests: Vec<Value>,
}

#[async_trait]
impl<P: Provider> Provider for Recording<P> {
    fn info(&self) -> ProviderInfo {
        self.inner.info()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.requests.push(json!({
            "turn": request.turn,
            "system": request.system,
            "tools": request.tools,
            "messages": request.messages,
            "max_output_tokens": request.max_output_tokens,
        }));
        self.inner.complete(request).await
    }
}

fn golden_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The base-set session the golden files were recorded from: every base
/// tool, and the calls a coding session makes, which the base set refuses.
fn base_script() -> ScriptedProvider {
    ScriptedProvider::new([
        ModelResponse::tool_use([ToolCall::new(
            "c1",
            "write_file",
            json!({"path": "notes.txt", "content": "one\ntwo\nthree\n"}),
        )]),
        ModelResponse::tool_use([
            ToolCall::new("c2", "read_file", json!({"path": "notes.txt"})),
            ToolCall::new(
                "c3",
                "read_file",
                json!({"path": "notes.txt", "start_line": 2, "line_count": 1}),
            ),
        ]),
        ModelResponse::tool_use([
            ToolCall::new(
                "c4",
                "edit_file",
                json!({"path": "notes.txt", "edits": [{"old_text": "two", "new_text": "2"}]}),
            ),
            ToolCall::new("c5", "search", json!({"pattern": "two"})),
        ]),
        ModelResponse::tool_use([
            ToolCall::new("c6", "list_dir", json!({"path": "."})),
            ToolCall::new("c7", "workspace_diff", json!({})),
        ]),
        ModelResponse::tool_use([ToolCall::new(
            "c8",
            "exec",
            json!({"argv": ["/usr/bin/wc", "-c", "/workspace/notes.txt"]}),
        )]),
        ModelResponse::final_text("done"),
    ])
}

fn base_config() -> HarnessConfig {
    HarnessConfig {
        max_steps: 12,
        max_tool_calls: 20,
        max_truncation_recoveries: 0,
        ..HarnessConfig::default()
    }
}

/// Run the base session and return its journal, normalized as the demo
/// golden is (fixed session ID, `unix_ms` and latency), and its requests.
async fn base_session() -> (String, String) {
    let config = base_config();
    let task = SessionTask::new(
        default_system_prompt(&config.tool_limits),
        "Change two to 2 in notes.txt.",
    );
    let mut harness = Harness::new(
        FakeApi::default(),
        Recording {
            inner: base_script(),
            requests: Vec::new(),
        },
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    harness
        .run(&task, CancellationToken::new())
        .await
        .expect("base session");
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
    let mut journal = common::chained_lines("golden-base", events).join("\n");
    journal.push('\n');
    let mut requests = harness
        .provider()
        .requests
        .iter()
        .map(|request| serde_json::to_string(request).expect("encode request"))
        .collect::<Vec<_>>()
        .join("\n");
    requests.push('\n');
    (journal, requests)
}

/// The base tool set is the default, and a base session's model requests
/// and journal are byte-for-byte what the build before the coding set
/// produced: the golden files were recorded from it.
#[tokio::test]
async fn base_session_requests_and_journal_match_the_golden_files() {
    let (journal, requests) = base_session().await;
    let golden_journal =
        std::fs::read_to_string(golden_path("base-tool-set-journal.jsonl")).expect("golden");
    let golden_requests =
        std::fs::read_to_string(golden_path("base-tool-set-requests.jsonl")).expect("golden");
    assert_eq!(requests, golden_requests);
    assert_eq!(journal, golden_journal);
}

#[test]
fn the_base_set_refuses_the_coding_calls_as_before() {
    let journal =
        std::fs::read_to_string(golden_path("base-tool-set-journal.jsonl")).expect("golden");
    let results = journal
        .lines()
        .map(|line| serde_json::from_str::<JournalRecord>(line).expect("record"))
        .filter_map(|record| match record.event {
            JournalEvent::ToolExecuted { call, result, .. } => {
                Some((call.call_id, call.tool, result))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let by_id = |id: &str| {
        results
            .iter()
            .find(|(call_id, _, _)| call_id == id)
            .expect("the call")
    };
    assert_eq!(
        by_id("c3").2,
        json!({"ok": false, "error": "invalid_arguments",
               "message": "unknown field `line_count`, expected `path` or `max_bytes`"})
    );
    for id in ["c4", "c5"] {
        assert_eq!(by_id(id).1, "unknown");
        assert_eq!(by_id(id).2["error"], "unknown_tool");
    }
    // The base tools are the default and are exactly those the journal
    // recorded.
    assert_eq!(HarnessConfig::default().tool_set, ToolSet::Base);
    let registry = ToolRegistry::new(ToolLimits::default());
    let names = |set: ToolSet| {
        registry
            .definitions_of(set, true)
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(ToolSet::Base),
        [
            "exec",
            "read_file",
            "write_file",
            "list_dir",
            "workspace_diff"
        ]
    );
    assert_eq!(
        names(ToolSet::CodingV1),
        [
            "exec",
            "read_file",
            "write_file",
            "list_dir",
            "workspace_diff",
            "edit_file",
            "search"
        ]
    );
    // Only read_file differs among the tools both sets have.
    for streaming in [false, true] {
        let base = registry.definitions_of(ToolSet::Base, streaming);
        let coding = registry.definitions_of(ToolSet::CodingV1, streaming);
        for (base, coding) in base.iter().zip(coding) {
            assert_eq!(base.name, coding.name);
            if base.name != "read_file" {
                assert_eq!(base, coding);
            }
        }
        assert_eq!(
            registry.definitions_of(ToolSet::Base, false),
            registry.definitions()
        );
    }
}

#[test]
fn tool_set_names_round_trip() {
    for set in [ToolSet::Base, ToolSet::CodingV1] {
        assert_eq!(ToolSet::from_journal(set.journal_name()), Some(set));
    }
    assert_eq!(ToolSet::Base.journal_name(), None);
    assert_eq!(ToolSet::CodingV1.journal_name(), Some("coding_v1"));
    assert_eq!(ToolSet::from_journal(Some("coding_v9")), None);
    assert_eq!(ToolSet::from_journal(Some("base")), None);
    assert_eq!(ToolSet::Base.report_name(), "base");
    assert_eq!(ToolSet::CodingV1.report_name(), "coding_v1");
}

// ---------------------------------------------------------------------------
// Coding sessions against the fake kilnd

fn coding_config() -> HarnessConfig {
    HarnessConfig {
        max_steps: 12,
        max_tool_calls: 64,
        tool_set: ToolSet::CodingV1,
        ..short_lifecycle_config()
    }
}

fn coding_task(config: &HarnessConfig) -> SessionTask {
    SessionTask::new(
        default_system_prompt_for(&config.tool_limits, ToolSet::CodingV1),
        "Change beta to BETA in notes.txt.",
    )
}

/// A fake kilnd whose workspace holds `files`, relative to `/workspace`.
fn api_with(files: &[(&str, &[u8])]) -> FakeApi {
    let api = FakeApi::default();
    {
        let mut state = api.state();
        for (path, contents) in files {
            state
                .files
                .insert(format!("/workspace/{path}"), contents.to_vec());
        }
    }
    api
}

fn turn(calls: Vec<(&str, &str, Value)>) -> ModelResponse {
    ModelResponse::tool_use(
        calls
            .into_iter()
            .map(|(id, tool, arguments)| ToolCall::new(id, tool, arguments)),
    )
}

/// Run `responses` as a session with `config` against `api`; the journal
/// and the results by call ID.
async fn session(
    api: &FakeApi,
    config: HarnessConfig,
    responses: Vec<ModelResponse>,
) -> (Vec<JournalRecord>, BTreeMap<String, Value>) {
    let task = coding_task(&config);
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new(responses),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    harness
        .run(&task, CancellationToken::new())
        .await
        .expect("the session completes");
    let records = harness.journal().records.clone();
    (records.clone(), tool_results(&records))
}

fn tool_results(records: &[JournalRecord]) -> BTreeMap<String, Value> {
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

fn coding_script() -> Vec<ModelResponse> {
    vec![
        turn(vec![
            (
                "r1",
                "read_file",
                json!({"path": "notes.txt", "start_line": 2, "line_count": 1}),
            ),
            ("r2", "read_file", json!({"path": "notes.txt"})),
        ]),
        turn(vec![(
            "e1",
            "edit_file",
            json!({"path": "notes.txt", "edits": [{"old_text": "beta", "new_text": "BETA"}]}),
        )]),
        turn(vec![
            ("s1", "search", json!({"pattern": "BETA"})),
            (
                "s2",
                "search",
                json!({"pattern": "a", "path": "lib", "max_results": 1}),
            ),
        ]),
        ModelResponse::final_text("done"),
    ]
}

const NOTES: &[u8] = b"alpha\nbeta\ngamma\n";

fn workspace() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("notes.txt", NOTES),
        ("lib/a.sh", b"a=1\nb=2\na=3\n"),
        ("lib/b.sh", b"say a\n"),
    ]
}

fn loaded(records: Vec<JournalRecord>) -> LoadedJournal {
    LoadedJournal {
        records,
        complete: true,
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_coding_session_records_its_tool_set_and_replays_matched() {
    let api = api_with(&workspace());
    let (records, results) = session(&api, coding_config(), coding_script()).await;
    let JournalEvent::SessionStarted {
        tool_set,
        tools,
        system,
        ..
    } = &records[0].event
    else {
        panic!("session start");
    };
    assert_eq!(tool_set.as_deref(), Some("coding_v1"));
    assert_eq!(
        tools.as_slice(),
        ToolRegistry::new(coding_config().tool_limits).definitions_of(ToolSet::CodingV1, true)
    );
    assert!(system.contains("- edit_file: "), "{system}");

    assert_eq!(
        results["r1"],
        json!({"ok": true, "content": "beta\n", "utf8": true, "file_bytes": NOTES.len(),
               "start_line": 2, "returned_lines": 1, "next_line": 3,
               "line_truncated": false, "truncated": true})
    );
    // Without a line field it is the base read.
    assert_eq!(
        results["r2"],
        json!({"ok": true, "content": "alpha\nbeta\ngamma\n", "utf8": true,
               "file_bytes": NOTES.len(), "returned_bytes": NOTES.len(), "truncated": false})
    );
    assert_eq!(
        results["e1"],
        json!({"ok": true, "replacements": 1, "file_bytes": NOTES.len()})
    );
    assert_eq!(
        api.state().files["/workspace/notes.txt"],
        b"alpha\nBETA\ngamma\n"
    );
    assert_eq!(
        results["s1"],
        json!({"ok": true, "matches": "notes.txt:2:BETA\n", "returned": 1, "seen": 1,
               "truncated": false, "skipped_files": 0, "unparsed": 0, "partial": false})
    );
    assert_eq!(
        results["s2"],
        json!({"ok": true, "matches": "lib/a.sh:1:a=1\n", "returned": 1, "seen": 3,
               "truncated": true, "skipped_files": 0, "unparsed": 0, "partial": false})
    );
    // The searches ran exactly the argv the design fixes.
    let greps = api
        .state()
        .execs
        .iter()
        .filter(|exec| exec.argv[0] == "/bin/grep")
        .map(|exec| exec.argv.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        greps[1],
        [
            "/bin/grep",
            "-n",
            "-H",
            "-F",
            "-e",
            "a",
            "--",
            "/workspace/lib/a.sh",
            "/workspace/lib/b.sh"
        ]
    );
    let finds = api
        .state()
        .execs
        .iter()
        .filter(|exec| exec.argv[0] == "/usr/bin/find")
        .map(|exec| exec.argv[1].clone())
        .collect::<Vec<_>>();
    assert_eq!(finds, ["/workspace", "/workspace", "/workspace/lib"]);

    // The journal verifies as a file, and replays to the same results.
    let temp = common::private_tempdir().expect("temp dir");
    let path = temp.path().join("coding.jsonl");
    let events = records.iter().map(|record| record.event.clone()).collect();
    common::write_journal_lines(&path, &common::chained_lines("hs-coding", events));
    let journal = read_journal(&path).expect("the coding journal verifies");
    let provider = ReplayProvider::from_journal(&journal).expect("replayable");
    assert_eq!(provider.tool_set(), ToolSet::CodingV1);
    let config = provider.replay_config(short_lifecycle_config());
    assert_eq!(config.tool_set, ToolSet::CodingV1);
    let replay_api = api_with(&workspace());
    let mut replay = Harness::new(
        replay_api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let outcome = replay
        .replay(CancellationToken::new())
        .await
        .expect("the replay completes");
    assert_eq!(outcome.final_answer, "done");
    assert_eq!(replay.provider().diverged_at(), None);
    assert!(replay.provider().exhausted());
    assert_eq!(tool_results(&replay.journal().records), results);
    assert_eq!(
        replay_api.state().files["/workspace/notes.txt"],
        b"alpha\nBETA\ngamma\n"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
        .args(["journal", "verify"])
        .arg(&path)
        .output()
        .expect("run journal verify");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("tool calls: 5\ntool set: coding_v1\n"),
        "{stdout}"
    );
}

#[tokio::test]
async fn a_replay_with_the_other_tool_set_is_refused_before_kilnd() {
    let (records, _) = session(&api_with(&workspace()), coding_config(), coding_script()).await;
    let provider = ReplayProvider::from_journal(&loaded(records)).expect("replayable");
    let mut config = provider.replay_config(short_lifecycle_config());
    config.tool_set = ToolSet::Base;
    let api = api_with(&workspace());
    let mut replay = Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let error = replay
        .replay(CancellationToken::new())
        .await
        .expect_err("refused");
    assert!(
        matches!(&error, HarnessError::InvalidTask(message)
            if message == "the tool set differs from the recording's; build the replay's \
                           configuration with ReplayProvider::replay_config"),
        "{error}"
    );
    assert!(api.state().creates.is_empty());
    assert_eq!(api.state().health_calls, 0);

    // And a base recording under the coding set.
    let golden = read_journal(&golden_path("base-tool-set-journal.jsonl")).expect("golden");
    let provider = ReplayProvider::from_journal(&golden).expect("replayable");
    assert_eq!(provider.tool_set(), ToolSet::Base);
    let mut config = provider.replay_config(short_lifecycle_config());
    assert_eq!(config.tool_set, ToolSet::Base);
    config.tool_set = ToolSet::CodingV1;
    let api = FakeApi::default();
    let mut replay = Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    assert!(matches!(
        replay.replay(CancellationToken::new()).await,
        Err(HarnessError::InvalidTask(_))
    ));
    assert!(api.state().creates.is_empty());
}

#[tokio::test]
async fn a_journal_naming_an_unknown_tool_set_is_refused() {
    let (records, _) = session(&api_with(&workspace()), coding_config(), coding_script()).await;
    let mut events = records
        .into_iter()
        .map(|record| record.event)
        .collect::<Vec<_>>();
    let JournalEvent::SessionStarted { tool_set, .. } = &mut events[0] else {
        panic!("session start");
    };
    *tool_set = Some("coding_v9".to_string());
    let temp = common::private_tempdir().expect("temp dir");
    let path = temp.path().join("v9.jsonl");
    common::write_journal_lines(&path, &common::chained_lines("hs-v9", events));
    match read_journal(&path) {
        Err(JournalError::UnknownToolSet { sequence: 0, name }) => assert_eq!(name, "coding_v9"),
        other => panic!("expected an unknown tool set, got {other:?}"),
    }
    let output = Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
        .args(["journal", "verify"])
        .arg(&path)
        .output()
        .expect("run journal verify");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid: journal record 0 names tool set \"coding_v9\""),
        "{stderr}"
    );
}

/// Every host-side refusal of the coding tools, with nothing sent to kilnd.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn coding_calls_are_checked_on_the_host() {
    let api = api_with(&workspace());
    let config = HarnessConfig {
        tool_limits: ToolLimits {
            max_file_bytes: 1024,
            ..ToolLimits::default()
        },
        ..coding_config()
    };
    let long = "x".repeat(1025);
    let edits = |count: usize| {
        (0..count)
            .map(|index| json!({"old_text": format!("o{index}"), "new_text": "n"}))
            .collect::<Vec<_>>()
    };
    let calls: Vec<(&str, Value, &str, &str)> = vec![
        (
            "read_file",
            json!({"path": "notes.txt", "start_line": 0}),
            "invalid_arguments",
            "start_line must be 1 to 10000000",
        ),
        (
            "read_file",
            json!({"path": "notes.txt", "start_line": 10_000_001}),
            "invalid_arguments",
            "start_line must be 1 to 10000000",
        ),
        (
            "read_file",
            json!({"path": "notes.txt", "line_count": 0}),
            "invalid_arguments",
            "line_count must be 1 to 5000",
        ),
        (
            "read_file",
            json!({"path": "notes.txt", "start_line": 1, "line_count": 5001}),
            "invalid_arguments",
            "line_count must be 1 to 5000",
        ),
        (
            "read_file",
            json!({"path": "notes.txt", "line_count": 1, "max_bytes": 0}),
            "invalid_arguments",
            "max_bytes is outside the per-file limit",
        ),
        (
            "read_file",
            json!({"path": "notes.txt", "line_count": 1, "max_bytes": 1025}),
            "invalid_arguments",
            "max_bytes is outside the per-file limit",
        ),
        (
            "read_file",
            json!({"path": "../notes.txt", "line_count": 1}),
            "invalid_path",
            "path components may not be empty, '.' or '..'",
        ),
        (
            "read_file",
            json!({"path": "notes.txt", "lines": 1}),
            "invalid_arguments",
            "unknown field `lines`, expected one of `path`, `max_bytes`, `start_line`, `line_count`",
        ),
        (
            "edit_file",
            json!({"path": "/workspace", "edits": edits(1)}),
            "invalid_path",
            "path names the workspace directory, not a file",
        ),
        (
            "edit_file",
            json!({"path": "notes.txt", "edits": []}),
            "invalid_arguments",
            "edits must hold 1 to 32 edits",
        ),
        (
            "edit_file",
            json!({"path": "notes.txt", "edits": edits(33)}),
            "invalid_arguments",
            "edits must hold 1 to 32 edits",
        ),
        (
            "edit_file",
            json!({"path": "notes.txt", "edits": [{"old_text": "", "new_text": "x"}]}),
            "invalid_arguments",
            "edit 0: old_text is empty",
        ),
        (
            "edit_file",
            json!({"path": "notes.txt", "edits": [{"old_text": "a", "new_text": "b"}, {"old_text": "c", "new_text": "c"}]}),
            "invalid_arguments",
            "edit 1: new_text equals old_text",
        ),
        (
            "edit_file",
            json!({"path": "notes.txt", "edits": [{"old_text": "a", "new_text": long}]}),
            "invalid_arguments",
            "edit 0: text exceeds the per-file limit",
        ),
        (
            "edit_file",
            json!({"path": "notes.txt", "edits": [{"old_text": long, "new_text": "a"}]}),
            "invalid_arguments",
            "edit 0: text exceeds the per-file limit",
        ),
        (
            "edit_file",
            json!({"path": "notes.txt", "edits": [{"old_text": "a", "new_text": "b", "all": true}]}),
            "invalid_arguments",
            "unknown field `all`, expected one of `old_text`, `new_text`, `replace_all`",
        ),
        (
            "search",
            json!({"pattern": ""}),
            "invalid_arguments",
            "pattern must be 1 to 1024 bytes with no newline or NUL",
        ),
        (
            "search",
            json!({"pattern": "x".repeat(1025)}),
            "invalid_arguments",
            "pattern must be 1 to 1024 bytes with no newline or NUL",
        ),
        (
            "search",
            json!({"pattern": "a\nb"}),
            "invalid_arguments",
            "pattern must be 1 to 1024 bytes with no newline or NUL",
        ),
        (
            "search",
            json!({"pattern": "a\u{0}b"}),
            "invalid_arguments",
            "pattern must be 1 to 1024 bytes with no newline or NUL",
        ),
        (
            "search",
            json!({"pattern": "a", "path": "/etc"}),
            "invalid_path",
            "path must be relative to /workspace or start with /workspace/, at most 1024 bytes",
        ),
        (
            "search",
            json!({"pattern": "a", "max_results": 0}),
            "invalid_arguments",
            "max_results must be 1 to 200",
        ),
        (
            "search",
            json!({"pattern": "a", "max_results": 201}),
            "invalid_arguments",
            "max_results must be 1 to 200",
        ),
    ];
    let mut responses = calls
        .chunks(8)
        .enumerate()
        .map(|(chunk, calls)| {
            turn(
                calls
                    .iter()
                    .enumerate()
                    .map(|(index, (tool, arguments, _, _))| {
                        (
                            Box::leak(format!("c{chunk}-{index}").into_boxed_str()) as &str,
                            *tool,
                            arguments.clone(),
                        )
                    })
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    responses.push(ModelResponse::final_text("done"));
    let (_, results) = session(&api, config, responses).await;
    for (number, (tool, arguments, kind, message)) in calls.iter().enumerate() {
        let result = &results[&format!("c{}-{}", number / 8, number % 8)];
        assert_eq!(
            result,
            &json!({"ok": false, "error": kind, "message": message}),
            "{tool} {arguments}"
        );
    }
    // Only the baseline reached kilnd.
    assert_eq!(api.state().execs.len(), 1);
    assert_eq!(api.state().files["/workspace/notes.txt"], NOTES);
}

fn incomplete_truncated(output: &mut CapturedOutput) {
    output.truncated = true;
    output.observed_bytes += 100;
}

fn incomplete_unknown(output: &mut CapturedOutput) {
    output.total_bytes_known = false;
}

fn incomplete_short(output: &mut CapturedOutput) {
    output.observed_bytes += 1;
}

#[tokio::test]
async fn an_edit_after_an_incomplete_read_writes_nothing() {
    for tamper in [
        incomplete_truncated as common::CaptureTamper,
        incomplete_unknown,
        incomplete_short,
    ] {
        let api = api_with(&workspace());
        api.state().capture_tamper = Some(("/usr/bin/head", tamper));
        let (_, results) = session(
            &api,
            coding_config(),
            vec![
                turn(vec![(
                    "e1",
                    "edit_file",
                    json!({"path": "notes.txt",
                           "edits": [{"old_text": "beta", "new_text": "BETA"}]}),
                )]),
                ModelResponse::final_text("done"),
            ],
        )
        .await;
        assert_eq!(
            results["e1"],
            json!({"ok": false, "error": "read_incomplete",
                   "message": "kilnd did not return the whole file; nothing was written"})
        );
        let state = api.state();
        assert_eq!(state.dd_count, 0, "no dd was issued");
        assert_eq!(state.files["/workspace/notes.txt"], NOTES);
        assert_eq!(
            state.execs.last().expect("the read").argv[0],
            "/usr/bin/head"
        );
    }
}

#[tokio::test]
async fn edit_refusals_after_the_read_keep_its_evidence() {
    let api = api_with(&[
        ("notes.txt", NOTES),
        ("big.txt", &[b'a'; 2048]),
        ("bin.dat", b"\xff\xfe"),
    ]);
    let config = HarnessConfig {
        tool_limits: ToolLimits {
            max_file_bytes: 1024,
            ..ToolLimits::default()
        },
        ..coding_config()
    };
    let one = |path: &str, old: &str, new: &str| json!({"path": path, "edits": [{"old_text": old, "new_text": new}]});
    let (records, results) = session(
        &api,
        config,
        vec![
            turn(vec![
                ("missing", "edit_file", one("gone.txt", "a", "b")),
                ("big", "edit_file", one("big.txt", "a", "b")),
                ("bin", "edit_file", one("bin.dat", "a", "b")),
                ("none", "edit_file", one("notes.txt", "delta", "b")),
                ("twice", "edit_file", one("notes.txt", "a", "b")),
                (
                    "grow",
                    "edit_file",
                    json!({"path": "notes.txt",
                           "edits": [{"old_text": "a", "new_text": "x".repeat(300), "replace_all": true}]}),
                ),
            ]),
            ModelResponse::final_text("done"),
        ],
    )
    .await;
    assert_eq!(
        results["missing"],
        json!({"ok": false, "error": "read_failed", "state": "exited", "exit_code": 1,
               "failure_reason": "", "stderr": "not found"})
    );
    let refusal =
        |kind: &str, message: &str| json!({"ok": false, "error": kind, "message": message});
    assert_eq!(
        results["big"],
        refusal("file_too_large", "the file exceeds 1 KiB; use exec")
    );
    assert_eq!(
        results["bin"],
        refusal("not_utf8", "edit_file edits UTF-8 text files only")
    );
    assert_eq!(
        results["none"],
        refusal("no_match", "edit 0: old_text was not found")
    );
    assert_eq!(
        results["twice"],
        refusal(
            "not_unique",
            "edit 0: old_text occurs more than once; include more surrounding text or set \
             replace_all"
        )
    );
    assert_eq!(
        results["grow"],
        refusal("file_too_large", "the edited file would exceed 1 KiB")
    );
    // Each kept its read's execution, and nothing was written.
    for record in &records {
        if let JournalEvent::ToolExecuted { execution_ids, .. } = &record.event {
            assert_eq!(execution_ids.len(), 1);
        }
    }
    assert_eq!(api.state().dd_count, 0);
    assert_eq!(api.state().files["/workspace/notes.txt"], NOTES);
}

#[tokio::test]
async fn a_failed_write_reports_both_executions() {
    let api = api_with(&workspace());
    api.state().fail_dd_from = Some(1);
    let (records, results) = session(
        &api,
        coding_config(),
        vec![
            turn(vec![(
                "e1",
                "edit_file",
                json!({"path": "notes.txt", "edits": [{"old_text": "beta", "new_text": "B"}]}),
            )]),
            ModelResponse::final_text("done"),
        ],
    )
    .await;
    assert_eq!(
        results["e1"],
        json!({"ok": false, "error": "write_failed", "state": "exited", "exit_code": 1,
               "failure_reason": "", "stderr": "dd: write error"})
    );
    let ids = records
        .iter()
        .find_map(|record| match &record.event {
            JournalEvent::ToolExecuted { execution_ids, .. } => Some(execution_ids.clone()),
            _ => None,
        })
        .expect("the call");
    assert_eq!(ids.len(), 2);
}

#[tokio::test]
async fn ranged_reads_scan_a_bounded_window() {
    // 2 MiB of numbered lines: the first MiB is scanned.
    let mut big = Vec::new();
    let mut line = 0u32;
    while big.len() < 2 << 20 {
        line += 1;
        big.extend_from_slice(format!("line {line:07}\n").as_bytes());
    }
    let per_line = "line 0000001\n".len();
    let known = (1 << 20) / per_line;
    let api = api_with(&[("big.txt", &big), ("tail.txt", b"a\r\nb\0c")]);
    let read = |start: usize, count: usize| json!({"path": "big.txt", "start_line": start, "line_count": count});
    let (_, results) = session(
        &api,
        coding_config(),
        vec![
            turn(vec![
                ("first", "read_file", read(1, 2)),
                ("last", "read_file", read(known, 5)),
                ("beyond", "read_file", read(known + 1, 1)),
                (
                    "tail",
                    "read_file",
                    json!({"path": "tail.txt", "start_line": 2}),
                ),
                (
                    "past",
                    "read_file",
                    json!({"path": "tail.txt", "start_line": 3}),
                ),
                (
                    "missing",
                    "read_file",
                    json!({"path": "gone.txt", "start_line": 1}),
                ),
            ]),
            ModelResponse::final_text("done"),
        ],
    )
    .await;
    assert_eq!(results["first"]["content"], "line 0000001\nline 0000002\n");
    assert_eq!(results["first"]["next_line"], 3);
    assert_eq!(results["first"]["file_bytes"], big.len());
    // The window ends mid-line: its last whole line is the last known one,
    // and more may follow.
    assert_eq!(
        results["last"]["content"],
        format!("line {known:07}\n").as_str()
    );
    assert_eq!(results["last"]["returned_lines"], 1);
    assert_eq!(results["last"]["next_line"], known + 1);
    assert_eq!(results["last"]["truncated"], true);
    assert_eq!(
        results["beyond"],
        json!({"ok": false, "error": "beyond_scan_limit",
               "message": "line ranges cover the first 1024 KiB of a file; use exec for later lines"})
    );
    assert_eq!(
        results["tail"],
        json!({"ok": true, "content": "b\u{0}c", "utf8": true, "file_bytes": 6,
               "start_line": 2, "returned_lines": 1, "next_line": null,
               "line_truncated": false, "truncated": false})
    );
    assert_eq!(results["past"]["returned_lines"], 0);
    assert_eq!(results["past"]["next_line"], Value::Null);
    assert_eq!(results["missing"]["error"], "stat_failed");
    let heads = api
        .state()
        .execs
        .iter()
        .filter(|exec| exec.argv[0] == "/usr/bin/head")
        .map(|exec| exec.argv.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        heads[0],
        ["/usr/bin/head", "-c", "1048576", "--", "/workspace/big.txt"]
    );
}

#[tokio::test]
async fn a_ranged_read_of_an_incomplete_capture_returns_only_whole_lines() {
    let api = api_with(&[("notes.txt", NOTES)]);
    api.state().capture_tamper = Some(("/usr/bin/head", incomplete_truncated));
    let (_, results) = session(
        &api,
        coding_config(),
        vec![
            turn(vec![
                (
                    "all",
                    "read_file",
                    json!({"path": "notes.txt", "start_line": 1}),
                ),
                (
                    "after",
                    "read_file",
                    json!({"path": "notes.txt", "start_line": 4}),
                ),
            ]),
            ModelResponse::final_text("done"),
        ],
    )
    .await;
    // Every line is whole, but the file may go on.
    assert_eq!(results["all"]["content"], "alpha\nbeta\ngamma\n");
    assert_eq!(results["all"]["next_line"], 4);
    assert_eq!(results["after"]["error"], "beyond_scan_limit");
}

fn search_workspace() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("b/two.txt", b"needle 2\nhay\nNeedle 3\n"),
        ("a/one.txt", b"needle 1\n"),
        ("a/z/deep.txt", b"no\nneedle deep\r\n"),
        ("c.txt", b"hay\n"),
        ("odd:name.txt", b"needle odd\n"),
    ]
}

async fn search_results(
    reversed: bool,
    calls: Vec<(&str, &str, Value)>,
) -> BTreeMap<String, Value> {
    let api = api_with(&search_workspace());
    api.state().find_reversed = reversed;
    let (_, results) = session(
        &api,
        coding_config(),
        vec![turn(calls), ModelResponse::final_text("done")],
    )
    .await;
    results
}

fn search_calls() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("all", "search", json!({"pattern": "needle"})),
        (
            "folded",
            "search",
            json!({"pattern": "NEEDLE", "ignore_case": true, "path": "b"}),
        ),
        (
            "file",
            "search",
            json!({"pattern": "needle", "path": "/workspace/a/one.txt"}),
        ),
        ("none", "search", json!({"pattern": "absent"})),
        (
            "missing",
            "search",
            json!({"pattern": "needle", "path": "nowhere"}),
        ),
        ("dash", "search", json!({"pattern": "-v"})),
        (
            "regex",
            "search",
            json!({"pattern": "need.e", "regex": true}),
        ),
    ]
}

#[tokio::test]
async fn search_results_are_sorted_whatever_the_enumeration_order() {
    let results = search_results(false, search_calls()).await;
    assert_eq!(results, search_results(true, search_calls()).await);
    assert_eq!(
        results["all"],
        json!({"ok": true,
               "matches": "a/one.txt:1:needle 1\na/z/deep.txt:2:needle deep\nb/two.txt:1:needle 2\n",
               "returned": 3, "seen": 3, "truncated": false, "skipped_files": 1,
               "unparsed": 0, "partial": false})
    );
    assert_eq!(
        results["folded"]["matches"],
        "b/two.txt:1:needle 2\nb/two.txt:3:Needle 3\n"
    );
    assert_eq!(results["file"]["matches"], "a/one.txt:1:needle 1\n");
    assert_eq!(results["none"]["matches"], "");
    assert_eq!(results["none"]["returned"], 0);
    assert_eq!(
        results["missing"],
        json!({"ok": false, "error": "search_failed", "state": "exited", "exit_code": 1,
               "failure_reason": "",
               "stderr": "find: /workspace/nowhere: No such file or directory\n"})
    );
    assert_eq!(results["dash"]["returned"], 0);
    // The fake matches literal strings only; -E reaches grep, which the
    // real-kilnd tests check.
    assert_eq!(results["regex"]["error"], "search_failed");
    assert_eq!(results["regex"]["stderr"], "fake: -E unsupported\n");
}

#[tokio::test]
async fn extended_regex_searches_send_dash_e_capital() {
    let api = api_with(&search_workspace());
    api.state().grep_handler = Some(Arc::new(
        |argv: &[String], _: &BTreeMap<String, Vec<u8>>| {
            assert_eq!(
                argv[..8],
                [
                    "/bin/grep",
                    "-n",
                    "-H",
                    "-E",
                    "-i",
                    "-e",
                    "ne+dle [0-9]",
                    "--"
                ]
            );
            ShAnswer {
                stdout: b"/workspace/a/one.txt:1:needle 1\n/workspace/elsewhere:1:x\nnoise\n"
                    .to_vec(),
                ..ShAnswer::exit(0)
            }
        },
    ));
    let (_, results) = session(
        &api,
        coding_config(),
        vec![
            turn(vec![(
                "regex",
                "search",
                json!({"pattern": "ne+dle [0-9]", "regex": true, "ignore_case": true}),
            )]),
            ModelResponse::final_text("done"),
        ],
    )
    .await;
    assert_eq!(
        results["regex"],
        json!({"ok": true, "matches": "a/one.txt:1:needle 1\n", "returned": 1, "seen": 1,
               "truncated": false, "skipped_files": 1, "unparsed": 2, "partial": false})
    );
}

#[tokio::test]
async fn a_grep_error_with_matches_is_a_partial_result() {
    let api = api_with(&search_workspace());
    api.state().grep_handler = Some(Arc::new(|_: &[String], _: &BTreeMap<String, Vec<u8>>| {
        ShAnswer {
            stdout: b"/workspace/a/one.txt:1:needle 1\n".to_vec(),
            stderr: b"grep: /workspace/c.txt: Permission denied\n".to_vec(),
            ..ShAnswer::exit(2)
        }
    }));
    let (_, results) = session(
        &api,
        coding_config(),
        vec![
            turn(vec![("s", "search", json!({"pattern": "needle"}))]),
            ModelResponse::final_text("done"),
        ],
    )
    .await;
    assert_eq!(results["s"]["partial"], true);
    assert_eq!(
        results["s"]["errors"],
        "grep: /workspace/c.txt: Permission denied\n"
    );
    assert_eq!(results["s"]["matches"], "a/one.txt:1:needle 1\n");
}

#[tokio::test]
async fn search_chunks_its_argv_and_stops_once_it_has_enough() {
    let names = (0..600)
        .map(|index| format!("f{index:03}.txt"))
        .collect::<Vec<_>>();
    let files = names
        .iter()
        .map(|name| (name.as_str(), &b"needle\n"[..]))
        .collect::<Vec<_>>();
    let api = api_with(&files);
    api.state()
        .files
        .insert("/workspace/huge.txt".to_string(), b"needle\n".repeat(10));
    let config = HarnessConfig {
        tool_limits: ToolLimits {
            max_manifest_entries: 1000,
            max_manifest_bytes: 1 << 20,
            ..ToolLimits::default()
        },
        ..coding_config()
    };
    let (_, results) = session(
        &api,
        config,
        vec![
            turn(vec![
                (
                    "few",
                    "search",
                    json!({"pattern": "needle", "max_results": 3}),
                ),
                (
                    "many",
                    "search",
                    json!({"pattern": "needle", "max_results": 200}),
                ),
            ]),
            ModelResponse::final_text("done"),
        ],
    )
    .await;
    assert_eq!(results["few"]["returned"], 3);
    assert_eq!(results["few"]["truncated"], true);
    assert_eq!(
        results["few"]["matches"],
        "f000.txt:1:needle\nf001.txt:1:needle\nf002.txt:1:needle\n"
    );
    assert_eq!(results["many"]["returned"], 200);
    let greps = api
        .state()
        .execs
        .iter()
        .filter(|exec| exec.argv[0] == "/bin/grep")
        .map(|exec| exec.argv.len() - 7)
        .collect::<Vec<_>>();
    // 601 files in chunks of 256; each search stopped after the first.
    assert_eq!(greps, [256, 256]);
}

#[tokio::test]
async fn too_many_files_is_refused_with_the_listing_as_evidence() {
    let api = api_with(&[("a.txt", b"x\n"), ("b.txt", b"x\n")]);
    let config = HarnessConfig {
        tool_limits: ToolLimits {
            max_manifest_entries: 3,
            ..ToolLimits::default()
        },
        ..coding_config()
    };
    let (records, results) = session(
        &api,
        config,
        vec![
            turn(vec![("fits", "search", json!({"pattern": "x"}))]),
            turn(vec![
                (
                    "c",
                    "write_file",
                    json!({"path": "c.txt", "content": "x\n"}),
                ),
                (
                    "d",
                    "write_file",
                    json!({"path": "d.txt", "content": "x\n"}),
                ),
            ]),
            turn(vec![("over", "search", json!({"pattern": "x"}))]),
            ModelResponse::final_text("done"),
        ],
    )
    .await;
    assert_eq!(results["fits"]["returned"], 2);
    assert_eq!(
        results["over"],
        json!({"ok": false, "error": "too_many_files",
               "message": "too many files to search; narrow path"})
    );
    let over = records
        .iter()
        .find_map(|record| match &record.event {
            JournalEvent::ToolExecuted {
                call,
                execution_ids,
                ..
            } if call.call_id == "over" => Some(execution_ids.clone()),
            _ => None,
        })
        .expect("the call");
    assert_eq!(over.len(), 1, "the listing is the refusal's evidence");
    assert_eq!(
        api.state()
            .execs
            .iter()
            .filter(|exec| exec.argv[0] == "/bin/grep")
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------
// Eval artifacts

fn write_file(path: &Path, contents: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create parents");
    std::fs::write(path, contents).expect("write");
}

fn eval_options(out: &Path, config: HarnessConfig) -> SuiteOptions {
    SuiteOptions {
        out: out.to_path_buf(),
        trials: 1,
        concurrency: 1,
        max_run_secs: None,
        effort: None,
        clear_thinking: None,
        pricing: None,
        unpriced_reason: None,
        replay: true,
        label: None,
        base_config: config,
        provider: ProviderInfo {
            name: "scripted".to_string(),
            model: "demo".to_string(),
        },
        base_url_host: None,
    }
}

#[tokio::test]
async fn an_eval_run_records_its_tool_set_and_tool_calls_by_name() {
    let temp = common::private_tempdir().expect("temp dir");
    let root = temp.path().join("tasks");
    let dir = root.join("edit-notes");
    write_file(
        &dir.join("task.json"),
        json!({"schema": 1, "id": "edit-notes", "description": "edit", "check_timeout_secs": 10})
            .to_string()
            .as_bytes(),
    );
    write_file(
        &dir.join("prompt.md"),
        b"Change beta to BETA in notes.txt.\n",
    );
    write_file(
        &dir.join("check.sh"),
        b"grep -q BETA /workspace/notes.txt\n",
    );
    write_file(
        &dir.join("solution.sh"),
        b"sed -i s/beta/BETA/ /workspace/notes.txt\n",
    );
    write_file(&dir.join("seed/notes.txt"), NOTES);
    let config = coding_config();
    let tasks = eval::load_tasks(&root, &[], &config).expect("loads");
    // The default system prompt follows the tool set.
    assert!(
        tasks[0]
            .session(&config)
            .system
            .contains("- search: find lines")
    );
    let api = FakeApi::default();
    api.state().sh_handler = Some(Arc::new(|_: &[u8], _: &mut BTreeMap<String, Vec<u8>>| {
        ShAnswer::exit(0)
    }));
    let out = temp.path().join("out");
    let summary = eval::run_suite(
        &api,
        &tasks,
        &eval_options(&out, config),
        || {
            Ok(ScriptedProvider::new([
                turn(vec![
                    (
                        "e1",
                        "edit_file",
                        json!({"path": "notes.txt",
                               "edits": [{"old_text": "beta", "new_text": "BETA"}]}),
                    ),
                    (
                        "r1",
                        "read_file",
                        json!({"path": "gone.txt", "start_line": 1}),
                    ),
                    ("x1", "frobnicate", json!({})),
                ]),
                ModelResponse::final_text("done"),
            ]))
        },
        &CancellationToken::new(),
        &mut |_| Ok(()),
    )
    .await
    .expect("the suite runs");
    assert_eq!(summary.stopped, None);
    let result = &summary.results[0];
    assert_eq!(result.check.as_deref(), Some("passed"));
    assert_eq!(result.replay.as_deref(), Some("matched"));
    assert_eq!(
        result.tool_calls_by_name,
        Some(BTreeMap::from([
            ("edit_file".to_string(), 1),
            ("read_file".to_string(), 1),
            ("unknown".to_string(), 1),
        ]))
    );
    assert_eq!(result.failed_tool_calls, Some(2));
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).expect("manifest"))
            .expect("manifest JSON");
    assert_eq!(manifest.tool_set, "coding_v1");
    let report = &summary.report;
    assert_eq!(report.tool_set, "coding_v1");
    assert_eq!(report.overall.failed_tool_calls, Some(2));
    let markdown = std::fs::read_to_string(out.join("report.md")).expect("markdown");
    assert!(markdown.contains("- Tool set: coding_v1\n"), "{markdown}");
    assert!(
        markdown.contains("\nTool calls by name: edit_file 1, read_file 1, unknown 1; failed 2.\n"),
        "{markdown}"
    );
    let line = std::fs::read_to_string(out.join("results.jsonl")).expect("results");
    assert!(
        line.contains(
            r#""tool_calls_by_name":{"edit_file":1,"read_file":1,"unknown":1},"failed_tool_calls":2"#
        ),
        "{line}"
    );
}

/// A results line and a manifest as written before tool sets and tool call
/// counts were recorded.
fn older_artifacts() -> (RunResult, Manifest) {
    let result = json!({
        "task": "a", "trial": 1, "session_id": "hs-a-1", "outcome": "completed",
        "work_outcome": "completed", "check": "passed", "check_exit_code": 0, "steps": 2,
        "tool_calls": 1, "input_tokens": 10, "output_tokens": 5, "wall_ms": 100,
        "replay": null, "error": null
    });
    let manifest = json!({
        "schema": 1, "harness_version": "0.1.0", "label": null, "provider": "scripted",
        "model": "demo", "base_url_host": null, "tasks": ["a"], "trials": 1, "replay": false,
        "started_unix_ms": 1, "ended_unix_ms": 2, "features": [], "stopped": null
    });
    (
        serde_json::from_value(result).expect("an older results line loads"),
        serde_json::from_value(manifest).expect("an older manifest loads"),
    )
}

#[test]
fn older_results_load_and_report_unknown_tool_calls() {
    let (result, manifest) = older_artifacts();
    assert_eq!(result.tool_calls_by_name, None);
    assert_eq!(result.failed_tool_calls, None);
    assert_eq!(manifest.tool_set, "base");
    let report = eval::report(&[result.clone()], &manifest);
    assert_eq!(report.tool_set, "base");
    assert_eq!(report.overall.tool_calls_by_name, None);
    assert_eq!(report.overall.failed_tool_calls, None);
    let markdown = eval::report_markdown(&report);
    assert!(markdown.contains("- Tool set: base\n"), "{markdown}");
    assert!(
        markdown.contains("\nTool calls by name: unknown.\n"),
        "{markdown}"
    );
    // Neither written when unknown, and an older report reads as base.
    let mut json = serde_json::to_value(&report).expect("encode");
    assert!(json["overall"].get("tool_calls_by_name").is_none());
    json.as_object_mut().expect("object").remove("tool_set");
    let back: Report = serde_json::from_value(json).expect("an older report loads");
    assert_eq!(back.tool_set, "base");
    // One run without counts makes the totals unknown.
    let mut counted = result.clone();
    counted.trial = 2;
    counted.tool_calls_by_name = Some(BTreeMap::from([("exec".to_string(), 1)]));
    counted.failed_tool_calls = Some(0);
    let mixed = eval::report(&[result, counted.clone()], &manifest);
    assert_eq!(mixed.overall.tool_calls_by_name, None);
    let known = eval::report(&[counted], &manifest);
    assert_eq!(
        known.overall.tool_calls_by_name,
        Some(BTreeMap::from([("exec".to_string(), 1)]))
    );
    assert!(eval::report_markdown(&known).contains("\nTool calls by name: exec 1; failed 0.\n"));
}

#[test]
fn compare_notes_a_tool_set_difference_and_both_sides_tool_calls() {
    let (result, manifest) = older_artifacts();
    let mut counted = result.clone();
    counted.tool_calls_by_name = Some(BTreeMap::from([("exec".to_string(), 3)]));
    counted.failed_tool_calls = Some(1);
    let base = eval::report(&[counted.clone()], &manifest);
    let mut coding_manifest = manifest.clone();
    coding_manifest.tool_set = "coding_v1".to_string();
    let mut coding_run = counted;
    coding_run.tool_calls_by_name = Some(BTreeMap::from([
        ("edit_file".to_string(), 2),
        ("search".to_string(), 1),
    ]));
    coding_run.failed_tool_calls = Some(0);
    let coding = eval::report(&[coding_run], &coding_manifest);
    let text = eval::compare(&base, &coding);
    assert!(
        text.contains(
            "Base tool calls by name: exec 3; failed 1.\nNew tool calls by name: edit_file 2, \
             search 1; failed 0.\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("\nTool sets differ: base → coding_v1; the model had other tools"),
        "{text}"
    );
    let same = eval::compare(&coding, &coding);
    assert!(!same.contains("Tool sets differ"), "{same}");
    // An older side's counts are unknown, so neither side's is shown; its
    // tool set is known to be base.
    let older = eval::report(&[result], &manifest);
    let text = eval::compare(&older, &coding);
    assert!(!text.contains("tool calls by name"), "{text}");
    assert!(
        text.contains("Tool sets differ: base → coding_v1"),
        "{text}"
    );
}

// ---------------------------------------------------------------------------
// The coding suite

fn coding_suite() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("evals/coding")
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// A coding task's seed in a host directory, with `/workspace` in its
/// check and solution rewritten to it, as the eval tests run the others.
struct HostTask {
    _dir: tempfile::TempDir,
    workspace: std::path::PathBuf,
    check: String,
    solution: String,
}

fn host_task(id: &str) -> HostTask {
    let task = coding_suite().join(id);
    let dir = common::private_tempdir().expect("tempdir");
    let workspace = dir.path().join("workspace");
    let seed = task.join("seed");
    for file in walk(&seed) {
        let target = workspace.join(file.strip_prefix(&seed).expect("under the seed"));
        write_file(&target, &std::fs::read(&file).expect("read"));
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

/// Run `script` as the sandbox runs a check, `/bin/sh -s` from `/`; true
/// when it exits 0.
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
fn the_coding_suite_loads_with_prompts_that_name_no_tool() {
    let tasks = eval::load_tasks(&coding_suite(), &[], &eval::selftest_config())
        .expect("the coding suite loads");
    assert_eq!(
        tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        ["config-sweep", "find-and-fix"]
    );
    let tools = ToolRegistry::new(ToolLimits::default());
    for task in &tasks {
        assert_eq!(task.check.timeout, std::time::Duration::from_secs(30));
        assert_eq!(
            task.session_timeout,
            Some(std::time::Duration::from_secs(300))
        );
        assert_eq!(task.max_steps, Some(30));
        let script = String::from_utf8(task.check.script.clone()).expect("UTF-8");
        assert!(
            script.starts_with("cd /workspace || exit 2\n"),
            "{}",
            task.id
        );
        // Both arms get the same prompt, so it names no tool.
        let words = task
            .prompt
            .split(|character: char| !(character.is_alphanumeric() || character == '_'))
            .collect::<Vec<_>>();
        for tool in tools.definitions_of(ToolSet::CodingV1, true) {
            assert!(
                !words.contains(&tool.name.as_str()),
                "{}: {}",
                task.id,
                tool.name
            );
        }
    }
    let config = &tasks[0].seed[0];
    assert_eq!(config.path, "settings.conf");
    assert!((55 << 10..=62 << 10).contains(&config.contents.len()));
    let lines = config.contents.split(|byte| *byte == b'\n').count() - 1;
    assert!((1400..=1700).contains(&lines), "{lines}");
    let project = &tasks[1].seed;
    let code = project
        .iter()
        .filter(|file| file.path.starts_with("lib/") || file.path.starts_with("bin/"))
        .count();
    assert!((25..=40).contains(&code), "{code}");
    let total: usize = project.iter().map(|file| file.contents.len()).sum();
    assert!(total <= 200 << 10, "{total}");
}

#[test]
fn each_coding_check_fails_bare_and_passes_after_its_solution_on_the_host() {
    for id in ["config-sweep", "find-and-fix"] {
        let task = host_task(id);
        assert!(!host_sh(&task.check), "{id}: the bare seed passed");
        assert!(host_sh(&task.solution), "{id}: the solution failed");
        assert!(host_sh(&task.check), "{id}: the solved check failed");
    }
}

#[test]
fn coding_checks_refuse_plausible_wrong_answers() {
    let run = |task: &HostTask, script: &str| {
        let script = format!("cd {} || exit 1\n{script}", task.workspace.display());
        assert!(host_sh(&script), "{script}");
    };
    let three = "awk '\
         index($0, \"database.key_311 = \") == 1 { print \"database.key_311 = 4096\"; next }\n\
         index($0, \"cache.key_842 = \") == 1 { print \"cache.key_842 = off\"; next }\n\
         index($0, \"auth.key_1204 = \") == 1 { print \"auth.key_1204 = strict\"; next }\n\
         { print }' settings.conf > s.new && mv s.new settings.conf";
    // config-sweep: the comment naming a key rewritten too; a backup left
    // behind; the deleted line kept; the deleted line left blank.
    for (solve, wrong) in [
        (
            true,
            "awk '{ sub(/^# cache\\.key_842 = on/, \"# cache.key_842 = off\") } { print }' \
             settings.conf > s.new && mv s.new settings.conf",
        ),
        (true, "cp settings.conf settings.conf.bak"),
        (false, three),
        (
            false,
            &format!(
                "{three} && awk '/^queue\\.key_555 = / {{ print \"\"; next }} {{ print }}' \
                 settings.conf > s.new && mv s.new settings.conf"
            ),
        ),
    ] {
        let task = host_task("config-sweep");
        if solve {
            assert!(host_sh(&task.solution));
        }
        run(&task, wrong);
        assert!(!host_sh(&task.check), "config-sweep passed after {wrong}");
    }
    // find-and-fix: the tests edited to pass, and the symptom patched in
    // days_in_month while is_leap_year stays wrong.
    let task = host_task("find-and-fix");
    run(
        &task,
        "awk '{ sub(/\"February 2000\" \"29\"/, \"\\\"February 2000\\\" \\\"28\\\"\") } \
         { sub(/\"valid_date\" \"yes\"/, \"\\\"valid_date\\\" \\\"no\\\"\") } { print }' \
         tests/run.sh > t.new && mv t.new tests/run.sh && sh tests/run.sh",
    );
    assert!(!host_sh(&task.check), "edited tests passed");
    let task = host_task("find-and-fix");
    run(
        &task,
        "awk '{ sub(/if is_leap_year \"\\$2\"; then/, \
         \"if is_leap_year \\\"$2\\\" || [ $(( $2 % 400 )) -eq 0 ]; then\") } { print }' \
         lib/dates.sh > d.new && mv d.new lib/dates.sh && sh tests/run.sh",
    );
    assert!(!host_sh(&task.check), "a fix outside is_leap_year passed");
    // The right fix, but a rewrite of calendar.sh that lost month_abbr, or
    // of another module that lost a function the tests do not call.
    for (file, from, to) in [
        ("lib/calendar.sh", "# month_abbr MONTH:", None),
        ("lib/paths.sh", "# path_dir PATH:", Some("# path_ext PATH:")),
    ] {
        let task = host_task("find-and-fix");
        assert!(host_sh(&task.solution));
        let path = task.workspace.join(file);
        let content = std::fs::read_to_string(&path).expect("read");
        let start = content.find(from).expect("the function");
        let end = to.map_or(content.len(), |to| content.find(to).expect("the next one"));
        std::fs::write(&path, format!("{}{}", &content[..start], &content[end..])).expect("write");
        let tests = format!("cd {} && sh tests/run.sh", task.workspace.display());
        assert!(
            host_sh(&tests),
            "{file}: the tests still pass without {from}"
        );
        assert!(
            !host_sh(&task.check),
            "the check passed without {file}'s {from}"
        );
    }
}

// ---------------------------------------------------------------------------
// Evidence of a composite tool's earlier executions

/// What the first tool call recorded: its result, its execution IDs in the
/// journal, and the audit's stdout bytes observed for it.
struct FirstCall {
    completed: bool,
    result: Value,
    execution_ids: Vec<String>,
    audited_ids: Vec<String>,
    stdout_observed: u64,
    stdout_known: bool,
}

async fn first_call(api: &FakeApi, config: HarnessConfig, call: (&str, Value)) -> FirstCall {
    let task = coding_task(&config);
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new([
            turn(vec![("first", call.0, call.1)]),
            ModelResponse::final_text("done"),
        ]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let completed = harness.run(&task, CancellationToken::new()).await.is_ok();
    let (result, execution_ids) = harness
        .journal()
        .records
        .iter()
        .find_map(|record| match &record.event {
            JournalEvent::ToolExecuted {
                result,
                execution_ids,
                ..
            } => Some((result.clone(), execution_ids.clone())),
            _ => None,
        })
        .expect("the call was journaled");
    let (audited_ids, stdout) = harness
        .audit()
        .records
        .iter()
        .find_map(|record| match &record.event {
            AuditEvent::ToolCompleted {
                execution_ids,
                stdout,
                ..
            } => Some((execution_ids.clone(), *stdout)),
            _ => None,
        })
        .expect("the call was audited");
    FirstCall {
        completed,
        result,
        execution_ids,
        audited_ids,
        stdout_observed: stdout.observed_bytes,
        stdout_known: stdout.total_bytes_known,
    }
}

fn ids(numbers: &[usize]) -> Vec<String> {
    numbers
        .iter()
        .map(|number| format!("ex-{number:016x}"))
        .collect()
}

fn clear_cwd(execution: &mut kiln_harness::api::Execution) {
    execution.cwd.clear();
}

/// The three composite tools, each with the 1-based number of its later
/// Exec (the baseline is 1), the IDs its earlier ones get, and the bytes
/// they wrote to stdout.
fn composite_calls() -> Vec<(&'static str, Value, usize, Vec<String>, u64)> {
    vec![
        (
            "read_file",
            json!({"path": "notes.txt", "start_line": 2}),
            3,
            ids(&[2]),
            // stat prints "17\n".
            3,
        ),
        (
            "edit_file",
            json!({"path": "notes.txt", "edits": [{"old_text": "beta", "new_text": "B"}]}),
            3,
            ids(&[2]),
            NOTES.len() as u64,
        ),
        (
            "search",
            json!({"pattern": "a"}),
            3,
            ids(&[2]),
            "/workspace/notes.txt\0".len() as u64,
        ),
    ]
}

#[tokio::test]
async fn a_later_context_mismatch_keeps_the_earlier_executions() {
    for (tool, arguments, later, earlier, earlier_stdout) in composite_calls() {
        let api = api_with(&[("notes.txt", NOTES)]);
        api.state().tamper_exec = Some((later, clear_cwd));
        let call = first_call(&api, coding_config(), (tool, arguments)).await;
        assert!(!call.completed, "{tool}: the session ends at the mismatch");
        let mut want = earlier.clone();
        want.extend(ids(&[later]));
        assert_eq!(call.execution_ids, want, "{tool}");
        assert_eq!(call.audited_ids, want, "{tool}");
        assert_eq!(call.result["error"], "kilnd_error", "{tool}");
        // The earlier output's evidence is kept; the refused one's is not
        // known.
        assert_eq!(call.stdout_observed, earlier_stdout, "{tool}");
        assert!(!call.stdout_known, "{tool}");
    }
}

#[tokio::test]
async fn a_later_lost_response_is_still_recovered_after_the_earlier_executions() {
    for (tool, arguments, later, earlier, earlier_stdout) in composite_calls() {
        let api = api_with(&[("notes.txt", NOTES)]);
        api.state().lose_exec_response = Some(later);
        let call = first_call(&api, coding_config(), (tool, arguments)).await;
        assert!(call.completed, "{tool}: recovery settled the lost row");
        let mut want = earlier.clone();
        want.extend(ids(&[later]));
        assert_eq!(call.execution_ids, want, "{tool}");
        assert_eq!(call.audited_ids, want, "{tool}");
        assert_eq!(call.result["error"], "kilnd_error", "{tool}");
        // Recovery read the lost row: its output is counted after the
        // earlier ones', and every total is known again.
        assert!(call.stdout_observed >= earlier_stdout, "{tool}");
        assert!(call.stdout_known, "{tool}");
    }
}

/// 300 files make two grep chunks: find is Exec 2, the chunks 3 and 4.
fn two_chunk_api() -> FakeApi {
    let names = (0..300)
        .map(|index| format!("f{index:03}.txt"))
        .collect::<Vec<_>>();
    let files = names
        .iter()
        .map(|name| (name.as_str(), &b"hay\n"[..]))
        .collect::<Vec<_>>();
    api_with(&files)
}

fn two_chunk_config() -> HarnessConfig {
    HarnessConfig {
        tool_limits: ToolLimits {
            max_manifest_entries: 500,
            ..ToolLimits::default()
        },
        ..coding_config()
    }
}

#[tokio::test]
async fn a_failed_later_search_chunk_keeps_the_listing_and_earlier_chunks() {
    let api = two_chunk_api();
    api.state().tamper_exec = Some((4, clear_cwd));
    let call = first_call(
        &api,
        two_chunk_config(),
        ("search", json!({"pattern": "needle"})),
    )
    .await;
    assert!(!call.completed);
    assert_eq!(call.execution_ids, ids(&[2, 3, 4]));

    let api = two_chunk_api();
    api.state().lose_exec_response = Some(4);
    let call = first_call(
        &api,
        two_chunk_config(),
        ("search", json!({"pattern": "needle"})),
    )
    .await;
    assert!(call.completed);
    assert_eq!(call.execution_ids, ids(&[2, 3, 4]));
    assert!(call.stdout_known);
}

#[tokio::test]
async fn nothing_is_sent_after_an_execution_leaves_the_sandbox_unusable() {
    fn unusable(execution: &mut kiln_harness::api::Execution) {
        execution.sandbox_reusable = Some(false);
    }
    let api = api_with(&[("notes.txt", NOTES)]);
    api.state().tamper_exec = Some((2, unusable));
    let call = first_call(
        &api,
        coding_config(),
        (
            "edit_file",
            json!({"path": "notes.txt", "edits": [{"old_text": "beta", "new_text": "B"}]}),
        ),
    )
    .await;
    assert!(!call.completed);
    assert_eq!(api.state().execs.len(), 2, "no dd was sent");
    assert_eq!(api.state().files["/workspace/notes.txt"], NOTES);
    assert_eq!(call.execution_ids, ids(&[2]));
    assert_eq!(call.stdout_observed, NOTES.len() as u64);
}

// ---------------------------------------------------------------------------
// Ranged reads through a symbolic link

fn linked(target: &[u8]) -> FakeApi {
    let api = api_with(&[("target.txt", target)]);
    api.state()
        .symlinks
        .insert("/workspace/link.txt".to_string(), "target.txt".to_string());
    api
}

#[tokio::test]
async fn a_ranged_read_through_a_symlink_measures_the_target() {
    for (target, start, content, returned) in [
        (&b"abc"[..], 1, "abc", 1),
        (b"abc\n", 1, "abc\n", 1),
        (b"abc", 2, "", 0),
        (b"abc\n", 2, "", 0),
    ] {
        let api = linked(target);
        let (_, results) = session(
            &api,
            coding_config(),
            vec![
                turn(vec![(
                    "r",
                    "read_file",
                    json!({"path": "link.txt", "start_line": start}),
                )]),
                ModelResponse::final_text("done"),
            ],
        )
        .await;
        assert_eq!(
            results["r"],
            json!({"ok": true, "content": content, "utf8": true, "file_bytes": target.len(),
                   "start_line": start, "returned_lines": returned, "next_line": null,
                   "line_truncated": false, "truncated": false}),
            "{target:?} from {start}"
        );
        let stat = api
            .state()
            .execs
            .iter()
            .find(|exec| exec.argv[0] == "/bin/stat")
            .map(|exec| exec.argv.clone())
            .expect("a stat");
        assert_eq!(
            stat,
            ["/bin/stat", "-L", "-c", "%s", "--", "/workspace/link.txt"]
        );
    }
    // The prefix read keeps the base argv, and with it the link's own size.
    let api = linked(b"abc");
    let (_, results) = session(
        &api,
        coding_config(),
        vec![
            turn(vec![("r", "read_file", json!({"path": "link.txt"}))]),
            ModelResponse::final_text("done"),
        ],
    )
    .await;
    assert_eq!(results["r"]["content"], "abc");
    assert_eq!(results["r"]["file_bytes"], "target.txt".len());
    assert_eq!(
        api.state().execs[1].argv,
        ["/bin/stat", "-c", "%s", "--", "/workspace/link.txt"]
    );
}
