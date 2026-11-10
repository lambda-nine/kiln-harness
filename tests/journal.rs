mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{FakeApi, short_lifecycle_config};
use kiln_harness::audit::MemoryAudit;
use kiln_harness::engine::{
    Harness, HarnessConfig, HarnessError, MAX_BYTE_LIMIT, MAX_OUTPUT_TOKENS_PER_REQUEST,
    MAX_SESSION_TIMEOUT, MAX_STEPS, MAX_TOOL_CALLS, MAX_TOOL_CALLS_PER_TURN, MAX_TOOL_TIMEOUT,
    MAX_TOTAL_TOKENS, SessionOutcome, SessionTask,
};
use kiln_harness::journal::{
    DirectoryProblem, GENESIS_SHA256, JournalError, JournalEvent, JournalLimits, JournalRecord,
    JournalSink, JsonlJournal, LoadedJournal, MAX_JOURNAL_BYTES, MAX_JOURNAL_LINE_BYTES,
    MemoryJournal, read_journal, read_journal_with_limits, request_sha256,
};
use kiln_harness::provider::{
    DEMO_CONTENT, Message, ModelRequest, ModelResponse, Provider, ProviderErrorKind,
    ScriptedProvider, ToolCall, ToolDefinition,
};
use kiln_harness::replay::ReplayProvider;
use kiln_harness::tools::ToolLimits;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("write to String");
            hex
        })
}

/// Record the demo against a fresh fake into `dir/name`.
async fn record_demo(dir: &Path, name: &str) -> (PathBuf, SessionOutcome) {
    let path = dir.join(name);
    let journal = JsonlJournal::open(&path).expect("open journal");
    let mut harness = Harness::new(
        FakeApi::default(),
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        journal,
        HarnessConfig::default(),
    );
    let outcome = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("demo succeeds");
    (path, outcome)
}

fn hash(system: &str, tools: &[ToolDefinition], messages: &[Message]) -> String {
    hash_with_cap(system, tools, messages, 4096)
}

fn hash_with_cap(
    system: &str,
    tools: &[ToolDefinition],
    messages: &[Message],
    max_output_tokens: u32,
) -> String {
    request_sha256(&ModelRequest {
        session_id: "hs-any",
        turn: 1,
        system,
        tools,
        messages,
        max_output_tokens,
    })
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("read journal")
        .lines()
        .map(str::to_string)
        .collect()
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(path, text).expect("write journal");
}

fn event_names(journal: &LoadedJournal) -> Vec<&'static str> {
    journal
        .records
        .iter()
        .map(|record| match record.event {
            JournalEvent::SessionStarted { .. } => "session_started",
            JournalEvent::SandboxReady { .. } => "sandbox_ready",
            JournalEvent::ModelRequested { .. } => "model_requested",
            JournalEvent::ModelResponded { .. } => "model_responded",
            JournalEvent::ModelFailed { .. } => "model_failed",
            JournalEvent::ToolExecuted { .. } => "tool_executed",
            JournalEvent::SessionEnded { .. } => "session_ended",
        })
        .collect()
}

#[tokio::test]
async fn demo_journal_chain_verifies_and_records_the_whole_session() {
    let temp = common::private_tempdir().expect("temp dir");
    let (path, outcome) = record_demo(temp.path(), "demo.jsonl").await;

    let journal = read_journal(&path).expect("journal verifies");

    assert!(journal.complete);
    assert_eq!(journal.session_id(), outcome.session_id);
    let request = ["model_requested", "model_responded"];
    let mut expected = vec!["session_started", "sandbox_ready"];
    for calls in [1, 2, 1, 1] {
        expected.extend(request);
        expected.extend(std::iter::repeat_n("tool_executed", calls));
    }
    expected.extend(request);
    expected.push("session_ended");
    assert_eq!(event_names(&journal), expected);
    assert!(matches!(
        &journal.records[0].event,
        JournalEvent::SessionStarted { schema: 1, provider, tools, limits, .. }
            if provider.name == "scripted"
                && tools.len() == 5
                && *limits == HarnessConfig::default().limits()
    ));
    assert!(matches!(
        &journal.records.last().expect("last").event,
        JournalEvent::SessionEnded {
            outcome: name,
            final_answer: Some(answer),
            steps: 5,
            tool_calls: 5,
            sandbox_deleted: true,
            ..
        } if name == "completed" && *answer == outcome.final_answer
    ));
    let indexes = journal
        .records
        .iter()
        .filter_map(|record| match &record.event {
            JournalEvent::ToolExecuted {
                turn,
                index,
                audit_call_id,
                ..
            } => Some((*turn, *index, audit_call_id.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        indexes,
        [
            (1, 1, "call-1-1".to_string()),
            (2, 1, "call-2-1".to_string()),
            (2, 2, "call-2-2".to_string()),
            (3, 1, "call-3-1".to_string()),
            (4, 1, "call-4-1".to_string()),
        ]
    );

    // The chain covers the exact line bytes.
    let raw = lines(&path);
    assert_eq!(raw.len(), journal.records.len());
    assert_eq!(journal.records[0].prev_sha256, GENESIS_SHA256);
    for (index, record) in journal.records.iter().enumerate().skip(1) {
        assert_eq!(record.sequence, index as u64);
        assert_eq!(record.prev_sha256, sha256_hex(raw[index - 1].as_bytes()));
    }
}

#[tokio::test]
async fn memory_journal_lines_form_the_same_chain() {
    let mut harness = Harness::new(
        FakeApi::default(),
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("demo succeeds");

    let journal = harness.journal();
    assert_eq!(journal.lines.len(), journal.records.len());
    for (index, record) in journal.records.iter().enumerate() {
        let parsed: JournalRecord = serde_json::from_str(&journal.lines[index]).expect("parse");
        assert_eq!(&parsed, record);
        let expected = if index == 0 {
            GENESIS_SHA256.to_string()
        } else {
            sha256_hex(journal.lines[index - 1].as_bytes())
        };
        assert_eq!(record.prev_sha256, expected);
    }
}

#[tokio::test]
async fn edited_middle_byte_breaks_the_chain() {
    let temp = common::private_tempdir().expect("temp dir");
    let (path, _) = record_demo(temp.path(), "demo.jsonl").await;
    let mut raw = lines(&path);
    // Keep the JSON valid so only the hash chain can catch the edit.
    assert!(raw[1].contains("sb-0000000000000001"));
    raw[1] = raw[1].replace("sb-0000000000000001", "sb-0000000000000002");
    let edited = temp.path().join("edited.jsonl");
    write_lines(&edited, &raw);

    let error = read_journal(&edited).expect_err("edit is detected");

    assert!(
        matches!(error, JournalError::ChainMismatch { sequence: 2 }),
        "{error}"
    );
}

#[tokio::test]
async fn dropped_middle_record_is_detected() {
    let temp = common::private_tempdir().expect("temp dir");
    let (path, _) = record_demo(temp.path(), "demo.jsonl").await;
    let mut raw = lines(&path);
    raw.remove(4);
    let dropped = temp.path().join("dropped.jsonl");
    write_lines(&dropped, &raw);

    let error = read_journal(&dropped).expect_err("gap is detected");

    assert!(
        matches!(
            error,
            JournalError::SequenceGap {
                line: 5,
                found: 5,
                expected: 4
            }
        ),
        "{error}"
    );
}

#[tokio::test]
async fn dropped_tail_reads_as_incomplete() {
    let temp = common::private_tempdir().expect("temp dir");
    let (path, _) = record_demo(temp.path(), "demo.jsonl").await;
    let mut raw = lines(&path);
    raw.truncate(raw.len() - 3);
    let truncated = temp.path().join("truncated.jsonl");
    write_lines(&truncated, &raw);

    let journal = read_journal(&truncated).expect("prefix still verifies");

    assert!(!journal.complete);
    assert_eq!(journal.records.len(), raw.len());
}

#[tokio::test]
async fn torn_final_line_is_rejected() {
    let temp = common::private_tempdir().expect("temp dir");
    let (path, _) = record_demo(temp.path(), "demo.jsonl").await;
    let mut bytes = std::fs::read(&path).expect("read");
    bytes.truncate(bytes.len() - 10);
    let torn = temp.path().join("torn.jsonl");
    std::fs::write(&torn, bytes).expect("write");

    let error = read_journal(&torn).expect_err("torn write is detected");

    assert!(
        matches!(error, JournalError::UnterminatedLine { .. }),
        "{error}"
    );
}

#[tokio::test]
async fn records_after_session_end_and_second_sessions_are_rejected() {
    let temp = common::private_tempdir().expect("temp dir");
    let (path, _) = record_demo(temp.path(), "demo.jsonl").await;
    let raw = lines(&path);
    let last = raw.last().expect("last line").clone();
    let mut record: JournalRecord = serde_json::from_str(&last).expect("parse");
    record.sequence = raw.len() as u64;
    record.prev_sha256 = sha256_hex(last.as_bytes());
    let mut extended = raw.clone();
    extended.push(serde_json::to_string(&record).expect("encode"));
    let after_end = temp.path().join("after-end.jsonl");
    write_lines(&after_end, &extended);

    let error = read_journal(&after_end).expect_err("records after the end");
    assert!(
        matches!(error, JournalError::RecordAfterEnd { .. }),
        "{error}"
    );

    let mut sessions = raw.clone();
    let mut other: JournalRecord = serde_json::from_str(&raw[1]).expect("parse");
    other.session_id = "hs-someone-else".to_string();
    sessions[1] = serde_json::to_string(&other).expect("encode");
    let mixed = temp.path().join("mixed.jsonl");
    write_lines(&mixed, &sessions);

    let error = read_journal(&mixed).expect_err("second session ID");
    assert!(
        matches!(error, JournalError::SessionMismatch { sequence: 1, .. }),
        "{error}"
    );
}

#[test]
fn existing_journal_path_is_refused() {
    let temp = common::private_tempdir().expect("temp dir");
    let path = temp.path().join("existing.jsonl");
    std::fs::write(&path, b"keep me\n").expect("write");

    let error = JsonlJournal::open(&path)
        .err()
        .expect("existing path is refused");

    assert!(
        matches!(&error, JournalError::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists),
        "{error}"
    );
    assert_eq!(std::fs::read(&path).expect("read"), b"keep me\n");
}

#[cfg(unix)]
#[test]
fn journal_file_is_private_and_parents_are_created() {
    use std::os::unix::fs::PermissionsExt;

    let temp = common::private_tempdir().expect("temp dir");
    let path = temp.path().join("nested/dir/journal.jsonl");
    let _journal = JsonlJournal::open(&path).expect("open journal");

    let mode = |path: &Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&temp.path().join("nested")), 0o700);
    assert_eq!(mode(&temp.path().join("nested/dir")), 0o700);
}

#[test]
fn file_and_line_limits_are_enforced() {
    assert_eq!(
        JournalLimits::default(),
        JournalLimits {
            max_file_bytes: 64 << 20,
            max_line_bytes: 8 << 20,
        }
    );
    let temp = common::private_tempdir().expect("temp dir");

    // A sparse file one byte over the default size limit.
    let large = temp.path().join("large.jsonl");
    std::fs::File::create(&large)
        .expect("create")
        .set_len(MAX_JOURNAL_BYTES + 1)
        .expect("extend");
    let error = read_journal(&large).expect_err("oversized file");
    assert!(
        matches!(error, JournalError::FileTooLarge { limit } if limit == MAX_JOURNAL_BYTES),
        "{error}"
    );

    // One line over the default line limit, terminated or not.
    for terminated in [true, false] {
        let mut bytes = vec![b'x'; MAX_JOURNAL_LINE_BYTES + 1];
        if terminated {
            bytes.push(b'\n');
        }
        let long = temp.path().join(format!("long-{terminated}.jsonl"));
        std::fs::write(&long, bytes).expect("write");
        let error = read_journal(&long).expect_err("oversized line");
        assert!(
            matches!(error, JournalError::LineTooLarge { line: 1, .. }),
            "{error}"
        );
    }

    // Tighter explicit limits apply to a valid journal.
    let small = temp.path().join("small.jsonl");
    std::fs::write(&small, b"{}\n{}\n").expect("write");
    let error = read_journal_with_limits(
        &small,
        JournalLimits {
            max_file_bytes: 5,
            max_line_bytes: 8,
        },
    )
    .expect_err("explicit file limit");
    assert!(
        matches!(error, JournalError::FileTooLarge { limit: 5 }),
        "{error}"
    );
}

#[test]
fn non_regular_and_empty_journals_are_rejected() {
    let temp = common::private_tempdir().expect("temp dir");
    let error = read_journal(temp.path()).expect_err("directory");
    assert!(matches!(error, JournalError::NotRegularFile), "{error}");

    let empty = temp.path().join("empty.jsonl");
    std::fs::write(&empty, b"").expect("write");
    let error = read_journal(&empty).expect_err("empty");
    assert!(matches!(error, JournalError::Empty), "{error}");
}

#[test]
fn first_record_must_start_the_session() {
    let record = JournalRecord {
        sequence: 0,
        unix_ms: 0,
        session_id: "hs-test".to_string(),
        prev_sha256: GENESIS_SHA256.to_string(),
        event: JournalEvent::SandboxReady {
            sandbox_id: "sb-1".to_string(),
            baseline_file_count: 0,
        },
    };
    let temp = common::private_tempdir().expect("temp dir");
    let path = temp.path().join("headless.jsonl");
    write_lines(&path, &[serde_json::to_string(&record).expect("encode")]);

    let error = read_journal(&path).expect_err("first record is not session_started");
    assert!(
        matches!(error, JournalError::MissingSessionStarted { sequence: 0 }),
        "{error}"
    );

    // Writers refuse the same journal.
    let mut journal = MemoryJournal::default();
    let error = journal
        .append("hs-test", record.event)
        .expect_err("sink refuses a headless journal");
    assert!(matches!(
        error,
        JournalError::MissingSessionStarted { sequence: 0 }
    ));
}

#[test]
fn unsupported_schema_is_rejected() {
    let record = JournalRecord {
        sequence: 0,
        unix_ms: 0,
        session_id: "hs-test".to_string(),
        prev_sha256: GENESIS_SHA256.to_string(),
        event: JournalEvent::SessionStarted {
            schema: 2,
            harness_version: "0.0.0".to_string(),
            provider: kiln_harness::provider::ProviderInfo {
                name: "test".to_string(),
                model: "test".to_string(),
            },
            system: String::new(),
            task: "t".to_string(),
            tools: Vec::new(),
            limits: HarnessConfig::default().limits(),
        },
    };
    let temp = common::private_tempdir().expect("temp dir");
    let path = temp.path().join("future.jsonl");
    write_lines(&path, &[serde_json::to_string(&record).expect("encode")]);

    let error = read_journal(&path).expect_err("future schema");
    assert!(
        matches!(
            error,
            JournalError::UnsupportedSchema {
                found: 2,
                expected: 1
            }
        ),
        "{error}"
    );
}

#[tokio::test]
async fn oversized_journal_record_ends_work_and_still_cleans_up() {
    let api = FakeApi::default();
    let observer = api.clone();
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([ModelResponse::final_text(
            "x".repeat(MAX_JOURNAL_LINE_BYTES + 1),
        )]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    let error = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("the response cannot be journaled");

    assert!(
        matches!(
            error,
            HarnessError::Journal(JournalError::LineTooLarge { .. })
        ),
        "{error}"
    );
    assert_eq!(observer.state().deleted.len(), 1);
    assert!(matches!(
        harness.journal().records.last().map(|record| &record.event),
        Some(JournalEvent::SessionEnded {
            sandbox_deleted: true,
            ..
        })
    ));
}

#[tokio::test]
async fn audit_stays_content_free_while_the_journal_carries_content() {
    let mut harness = Harness::new(
        FakeApi::default(),
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );
    let outcome = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("demo succeeds");

    let audit = serde_json::to_string(&harness.audit().records).expect("serialize audit");
    let journal = harness.journal().lines.join("\n");
    let escaped_content = serde_json::to_string(DEMO_CONTENT).expect("escape");
    let escaped_content = escaped_content.trim_matches('"');
    for content in [
        escaped_content,
        SessionTask::demo().prompt.as_str(),
        outcome.final_answer.as_str(),
    ] {
        assert!(!audit.contains(content), "audit leaked {content:?}");
        assert!(journal.contains(content), "journal lacks {content:?}");
    }
}

#[test]
fn request_hash_is_independent_of_object_insertion_order() {
    let mut forward = Map::new();
    forward.insert("alpha".to_string(), json!(1));
    forward.insert("beta".to_string(), json!({"y": 2, "x": 1}));
    let mut reverse = Map::new();
    reverse.insert("beta".to_string(), json!({"x": 1, "y": 2}));
    reverse.insert("alpha".to_string(), json!(1));
    let messages = |arguments: Map<String, Value>| {
        vec![
            Message::User {
                text: "go".to_string(),
            },
            Message::Assistant {
                text: None,
                tool_calls: vec![ToolCall::new("c", "exec", Value::Object(arguments))],
            },
        ]
    };
    let tools: Vec<ToolDefinition> = Vec::new();

    // Fails if serde_json's preserve_order feature is ever enabled.
    assert_eq!(
        serde_json::to_string(&Value::Object(reverse.clone())).expect("encode"),
        r#"{"alpha":1,"beta":{"x":1,"y":2}}"#
    );
    assert_eq!(
        hash("system", &tools, &messages(forward)),
        hash("system", &tools, &messages(reverse.clone()))
    );
    assert_ne!(
        hash("system", &tools, &messages(reverse.clone())),
        hash("other", &tools, &messages(reverse))
    );
}

#[test]
fn request_hash_survives_a_journal_round_trip() {
    // Without serde_json's float_roundtrip feature, about a third of all
    // finite f64 values parse back one ulp off, and replay would diverge on
    // any float in model arguments or tool results.
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut floats = vec![json!(1.071_566_039_146_582_6e-75), json!(-0.0)];
    while floats.len() < 20_000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let value = f64::from_bits(state);
        if value.is_finite() {
            floats.push(json!(value));
        }
    }
    let arguments = json!({
        "floats": floats,
        "integers": [u64::MAX, i64::MIN, 0],
        "text": "line\nbreak \u{2603} \"quoted\""
    });
    let messages = vec![Message::Assistant {
        text: Some("t".to_string()),
        tool_calls: vec![ToolCall::new("c", "exec", arguments)],
    }];
    let encoded = serde_json::to_string(&messages).expect("encode");
    let decoded: Vec<Message> = serde_json::from_str(&encoded).expect("decode");

    assert_eq!(serde_json::to_string(&decoded).expect("re-encode"), encoded);
    assert_eq!(hash("s", &[], &messages), hash("s", &[], &decoded));
}

#[tokio::test]
async fn replay_reproduces_the_recorded_demo_against_a_fresh_sandbox() {
    let temp = common::private_tempdir().expect("temp dir");
    let (path, recorded) = record_demo(temp.path(), "demo.jsonl").await;
    let journal = read_journal(&path).expect("journal verifies");
    let provider = ReplayProvider::from_journal(&journal).expect("replayable");
    assert!(!provider.exhausted());
    assert_eq!(provider.task(), SessionTask::demo());
    assert_eq!(provider.limits(), HarnessConfig::default().limits());
    assert_eq!(
        provider.recorded_final_answer(),
        Some(recorded.final_answer.as_str())
    );
    let task = provider.task();
    let mut config = short_lifecycle_config();
    config.apply_limits(&provider.limits());

    let api = FakeApi::default();
    let observer = api.clone();
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let replayed = harness
        .run(&task, CancellationToken::new())
        .await
        .expect("replay reproduces");

    assert_eq!(replayed.final_answer, recorded.final_answer);
    assert_ne!(replayed.session_id, recorded.session_id);
    assert_eq!(replayed.steps, recorded.steps);
    assert_eq!(replayed.tool_calls, recorded.tool_calls);
    assert_eq!(harness.provider().diverged_at(), None);
    assert!(harness.provider().exhausted());
    assert_eq!(observer.state().deleted.len(), 1);
    let hashes = |records: &[JournalRecord]| {
        records
            .iter()
            .filter_map(|record| match &record.event {
                JournalEvent::ModelRequested { request_sha256, .. } => Some(request_sha256.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(hashes(&harness.journal().records), hashes(&journal.records));
}

#[tokio::test]
async fn replay_against_a_different_sandbox_diverges_and_cleans_up() {
    let temp = common::private_tempdir().expect("temp dir");
    let (path, _) = record_demo(temp.path(), "demo.jsonl").await;
    let journal = read_journal(&path).expect("journal verifies");
    let provider = ReplayProvider::from_journal(&journal).expect("replayable");
    let task = provider.task();

    let api = FakeApi::default();
    let observer = api.clone();
    observer.state().read_override = Some(b"goodbye from a different sandbox\n".to_vec());
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );
    let error = harness
        .run(&task, CancellationToken::new())
        .await
        .expect_err("different read_file content diverges");

    assert!(
        matches!(
            &error,
            HarnessError::Provider(provider) if provider.kind == ProviderErrorKind::ReplayDiverged
        ),
        "{error}"
    );
    // read_file runs in turn 2. Replay compares each tool result with the
    // recording as it completes, so it diverges there, before turn 3.
    assert_eq!(harness.provider().diverged_at(), Some(2));
    assert!(error.to_string().contains("tool call 1"), "{error}");
    assert_eq!(observer.state().deleted.len(), 1);
    assert!(matches!(
        harness.journal().records.last().map(|record| &record.event),
        Some(JournalEvent::SessionEnded {
            sandbox_deleted: true,
            ..
        })
    ));
}

#[tokio::test]
async fn replay_reproduces_recorded_provider_failures_and_stops_at_a_missing_tail() {
    let temp = common::private_tempdir().expect("temp dir");

    // A recorded provider failure is returned again at the same turn.
    let failed_path = temp.path().join("failed.jsonl");
    let mut harness = Harness::new(
        FakeApi::default(),
        ScriptedProvider::new([]),
        MemoryAudit::default(),
        JsonlJournal::open(&failed_path).expect("open"),
        short_lifecycle_config(),
    );
    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("empty script fails");
    let journal = read_journal(&failed_path).expect("verifies");
    let provider = ReplayProvider::from_journal(&journal).expect("replayable");
    assert_eq!(provider.recorded_outcome(), Some("failed"));
    let mut harness = Harness::new(
        FakeApi::default(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );
    let error = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("recorded failure is replayed");
    assert!(
        matches!(&error, HarnessError::Provider(provider) if provider.kind == ProviderErrorKind::Script),
        "{error}"
    );
    assert_eq!(harness.provider().diverged_at(), None);

    // A recording cut off during turn 3's provider call ends replay there.
    let (path, _) = record_demo(temp.path(), "demo.jsonl").await;
    let raw = lines(&path);
    let journal = read_journal(&path).expect("verifies");
    let cut = journal
        .records
        .iter()
        .position(|record| matches!(record.event, JournalEvent::ModelRequested { turn: 3, .. }))
        .expect("turn 3 request");
    let truncated = temp.path().join("truncated.jsonl");
    write_lines(&truncated, &raw[..=cut]);
    let journal = read_journal(&truncated).expect("prefix verifies");
    assert!(!journal.complete);
    let provider = ReplayProvider::from_journal(&journal).expect("replayable prefix");
    assert_eq!(provider.recorded_final_answer(), None);
    let mut harness = Harness::new(
        FakeApi::default(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );
    let error = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("no recorded response for turn 3");
    assert!(
        matches!(&error, HarnessError::Provider(provider)
            if provider.kind == ProviderErrorKind::Script && provider.message.contains("turn 3")),
        "{error}"
    );
}

#[test]
fn request_hash_covers_the_output_cap_but_not_session_or_turn() {
    let messages = vec![Message::User {
        text: "task".to_string(),
    }];
    let request = |session_id, turn, max_output_tokens| ModelRequest {
        session_id,
        turn,
        system: "system",
        tools: &[],
        messages: &messages,
        max_output_tokens,
    };
    let base = request_sha256(&request("hs-a", 1, 4096));
    assert_ne!(base, request_sha256(&request("hs-a", 1, 1)));
    assert_eq!(base, request_sha256(&request("hs-b", 1, 4096)));
    assert_eq!(base, request_sha256(&request("hs-a", 7, 4096)));
    assert_eq!(base, hash_with_cap("system", &[], &messages, 4096));
}

#[tokio::test]
async fn replay_diverges_when_only_the_output_cap_differs() {
    let temp = common::private_tempdir().expect("temp dir");
    let (path, _) = record_demo(temp.path(), "demo.jsonl").await;
    let journal = read_journal(&path).expect("journal verifies");
    let mut provider = ReplayProvider::from_journal(&journal).expect("replayable");
    let task = provider.task();
    let messages = vec![Message::User {
        text: task.prompt.clone(),
    }];
    let tools = kiln_harness::tools::ToolRegistry::new(kiln_harness::tools::ToolLimits::default())
        .definitions()
        .to_vec();

    let error = provider
        .complete(ModelRequest {
            session_id: "hs-new",
            turn: 1,
            system: &task.system,
            tools: &tools,
            messages: &messages,
            // The recording used the default cap of 4096.
            max_output_tokens: 1,
        })
        .await
        .expect_err("a different output cap is a different request");

    assert_eq!(error.kind, ProviderErrorKind::ReplayDiverged);
    assert_eq!(provider.diverged_at(), Some(1));
}

fn distinct_config() -> HarnessConfig {
    HarnessConfig {
        max_steps: 9,
        max_tool_calls: 11,
        max_tool_calls_per_turn: 3,
        session_timeout: Duration::from_millis(12_345),
        lifecycle_timeout: Duration::from_millis(250),
        lifecycle_retry_delay: Duration::from_millis(7),
        max_model_arguments_bytes: 1,
        max_final_answer_bytes: 1000,
        max_output_tokens_per_request: 77,
        max_total_input_tokens: 5555,
        max_total_output_tokens: 666,
        tool_limits: ToolLimits {
            timeout: Duration::from_millis(1234),
            max_argv_bytes: 101,
            max_stdin_bytes: 102,
            max_file_bytes: 103,
            max_result_bytes: 1040,
            max_manifest_bytes: 105,
            max_manifest_entries: 106,
        },
    }
}

#[test]
fn limits_round_trip_every_model_visible_setting() {
    let recorded = distinct_config();
    let mut restored = HarnessConfig::default();
    let lifecycle = (restored.lifecycle_timeout, restored.lifecycle_retry_delay);
    let limits = recorded.limits();

    // Through JSON, as replay reads them.
    let decoded =
        serde_json::from_str(&serde_json::to_string(&limits).expect("encode")).expect("decode");
    restored.apply_limits(&decoded);

    assert_eq!(restored.limits(), limits);
    assert_eq!(restored.max_model_arguments_bytes, 1);
    assert_eq!(restored.max_final_answer_bytes, 1000);
    assert_eq!(restored.session_timeout, recorded.session_timeout);
    assert_eq!(restored.tool_limits.timeout, recorded.tool_limits.timeout);
    assert_eq!(
        restored.tool_limits.max_manifest_entries,
        recorded.tool_limits.max_manifest_entries
    );
    assert_eq!(
        (restored.lifecycle_timeout, restored.lifecycle_retry_delay),
        lifecycle,
        "lifecycle windows are not model-visible and are not replayed"
    );
    let mut unknown = serde_json::to_value(limits).expect("value");
    unknown["surprise"] = json!(1);
    assert!(serde_json::from_value::<kiln_harness::journal::SessionLimits>(unknown).is_err());
}

#[tokio::test]
async fn replay_restores_a_recorded_argument_limit_and_reproduces() {
    let responses = [
        ModelResponse::tool_use([ToolCall::new("c", "workspace_diff", json!({}))]),
        ModelResponse::final_text("done"),
    ];
    let recording_api = FakeApi::default();
    let recording_observer = recording_api.clone();
    let mut recording_config = short_lifecycle_config();
    recording_config.max_model_arguments_bytes = 1;
    let mut recording = Harness::new(
        recording_api,
        ScriptedProvider::new(responses),
        MemoryAudit::default(),
        MemoryJournal::default(),
        recording_config,
    );
    recording
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("recording succeeds with an oversized-argument rejection");
    assert_eq!(
        recording_observer.state().execs.len(),
        1,
        "only the baseline"
    );

    let loaded = LoadedJournal {
        records: recording.journal().records.clone(),
        complete: true,
    };
    let provider = ReplayProvider::from_journal(&loaded).expect("replayable");
    let task = provider.task();
    let mut config = short_lifecycle_config();
    config.apply_limits(&provider.limits());
    assert_eq!(config.max_model_arguments_bytes, 1);
    let replay_api = FakeApi::default();
    let replay_observer = replay_api.clone();
    let mut replay = Harness::new(
        replay_api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );

    let outcome = replay
        .run(&task, CancellationToken::new())
        .await
        .expect("replay reproduces under the recorded limit");

    assert_eq!(outcome.final_answer, "done");
    assert_eq!(replay.provider().diverged_at(), None);
    assert_eq!(
        replay_observer.state().execs.len(),
        1,
        "the tool stays rejected"
    );
}

#[tokio::test]
async fn recorded_limits_above_the_hard_ceilings_are_rejected_before_create() {
    let temp = common::private_tempdir().expect("temp dir");
    let (path, _) = record_demo(temp.path(), "demo.jsonl").await;
    let journal = read_journal(&path).expect("journal verifies");
    let over: [fn(&mut kiln_harness::journal::SessionLimits); 8] = [
        |limits| limits.max_steps = MAX_STEPS + 1,
        |limits| limits.max_tool_calls = MAX_TOOL_CALLS + 1,
        |limits| limits.max_tool_calls_per_turn = MAX_TOOL_CALLS_PER_TURN + 1,
        |limits| limits.session_timeout_ms = 6 * 60 * 60 * 1000 + 1,
        |limits| limits.tool_limits.timeout_ms = 10 * 60 * 1000 + 1,
        |limits| limits.tool_limits.max_stdin_bytes = MAX_BYTE_LIMIT + 1,
        |limits| limits.max_output_tokens_per_request = MAX_OUTPUT_TOKENS_PER_REQUEST + 1,
        |limits| limits.max_total_input_tokens = MAX_TOTAL_TOKENS + 1,
    ];
    for raise in over {
        // A crafted journal with these limits fails verification and does not
        // load for replay.
        let mut crafted = journal.clone();
        let JournalEvent::SessionStarted { limits, .. } = &mut crafted.records[0].event else {
            panic!("first record starts the session");
        };
        raise(limits);
        assert!(ReplayProvider::from_journal(&crafted).is_err());
        let path = temp.path().join("crafted.jsonl");
        let session = crafted.session_id().to_string();
        let events = crafted
            .records
            .into_iter()
            .map(|record| record.event)
            .collect();
        common::write_journal_lines(&path, &common::chained_lines(&session, events));
        assert!(
            matches!(
                read_journal(&path),
                Err(JournalError::Grammar { sequence: 0, .. })
            ),
            "over-ceiling limits verify"
        );

        // And the engine refuses the limits themselves before any sandbox.
        let mut limits = HarnessConfig::default().limits();
        raise(&mut limits);
        let mut config = short_lifecycle_config();
        config.apply_limits(&limits);
        let task = SessionTask::demo();
        let api = FakeApi::default();
        let observer = api.clone();
        let mut replay = Harness::new(
            api,
            ScriptedProvider::demo(),
            MemoryAudit::default(),
            MemoryJournal::default(),
            config,
        );

        let error = replay
            .run(&task, CancellationToken::new())
            .await
            .expect_err("limits above a ceiling are invalid");

        assert!(
            matches!(error, HarnessError::InvalidModelCall(_)),
            "{error}"
        );
        assert!(observer.state().creates.is_empty());
    }
}

#[tokio::test]
async fn limits_exactly_at_the_ceilings_are_accepted() {
    let config = HarnessConfig {
        max_steps: MAX_STEPS,
        max_tool_calls: MAX_TOOL_CALLS,
        max_tool_calls_per_turn: MAX_TOOL_CALLS_PER_TURN,
        session_timeout: MAX_SESSION_TIMEOUT,
        max_model_arguments_bytes: MAX_BYTE_LIMIT,
        max_final_answer_bytes: MAX_BYTE_LIMIT,
        max_output_tokens_per_request: MAX_OUTPUT_TOKENS_PER_REQUEST,
        max_total_input_tokens: MAX_TOTAL_TOKENS,
        max_total_output_tokens: MAX_TOTAL_TOKENS,
        tool_limits: ToolLimits {
            timeout: MAX_TOOL_TIMEOUT,
            max_argv_bytes: MAX_BYTE_LIMIT,
            max_stdin_bytes: MAX_BYTE_LIMIT,
            max_file_bytes: MAX_BYTE_LIMIT,
            max_result_bytes: MAX_BYTE_LIMIT,
            max_manifest_bytes: MAX_BYTE_LIMIT,
            max_manifest_entries: 512,
        },
        ..short_lifecycle_config()
    };
    let mut harness = Harness::new(
        FakeApi::default(),
        ScriptedProvider::demo(),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );

    harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect("ceilings are inclusive");
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

#[test]
fn journal_is_created_in_the_resolved_parent_only_when_it_is_safe() {
    let temp = common::private_tempdir().expect("temp dir");
    let base = temp.path();

    // A symlinked parent resolves to its operator-owned target, which passes.
    let redirected = base.join("redirected");
    common::dir_all_with_mode(&redirected, 0o700);
    std::os::unix::fs::symlink(&redirected, base.join("link")).expect("symlink");
    let _journal = JsonlJournal::open(&base.join("link/session.jsonl")).expect("safe target");
    assert!(redirected.join("session.jsonl").is_file());

    // A symlinked parent that resolves somewhere writable by others fails,
    // and nothing is left behind.
    let shared = base.join("shared");
    common::dir_all_with_mode(&shared, 0o777);
    std::os::unix::fs::symlink(&shared, base.join("shared-link")).expect("symlink");
    let error = JsonlJournal::open(&base.join("shared-link/session.jsonl"))
        .err()
        .expect("unsafe target");
    assert!(
        matches!(&error, JournalError::UnsafeDirectory { path, .. } if path.ends_with("shared")),
        "{error}"
    );
    assert!(!shared.join("session.jsonl").exists());

    for (mode, safe) in [(0o700, true), (0o750, true), (0o770, false), (0o702, false)] {
        let parent = base.join(format!("parent-{mode:o}"));
        common::dir_all_with_mode(&parent, mode);
        let result = JsonlJournal::open(&parent.join("session.jsonl"));
        assert_eq!(result.is_ok(), safe, "parent mode {mode:o}");
        if let Err(error) = &result {
            // The refusal names the directory, its mode, and the fix.
            let message = error.to_string();
            assert!(message.contains(&parent.display().to_string()), "{message}");
            assert!(message.contains(&format!("mode {mode:04o}")), "{message}");
            assert!(message.contains("chmod g-w,o-w"), "{message}");
            assert!(message.contains("--journal"), "{message}");
            assert!(
                matches!(
                    error,
                    JournalError::UnsafeDirectory {
                        mode: Some(found),
                        problem: DirectoryProblem::WritableByOthers,
                        ..
                    } if found & 0o7777 == mode
                ),
                "{error:?}"
            );
        }
        assert_eq!(parent.join("session.jsonl").exists(), safe);
        set_mode(&parent, 0o700);
    }
}

#[test]
fn writable_ancestors_need_the_sticky_bit() {
    let temp = common::private_tempdir().expect("temp dir");
    for (mode, safe) in [(0o777, false), (0o1777, true), (0o755, true)] {
        let ancestor = temp.path().join(format!("ancestor-{mode:o}"));
        let parent = ancestor.join("private");
        common::dir_all_with_mode(&parent, 0o700);
        set_mode(&ancestor, mode);

        let result = JsonlJournal::open(&parent.join("session.jsonl"));

        assert_eq!(result.is_ok(), safe, "ancestor mode {mode:o}");
        assert_eq!(parent.join("session.jsonl").exists(), safe);
        set_mode(&ancestor, 0o700);
    }
}

#[test]
fn journal_path_must_name_a_file() {
    let temp = common::private_tempdir().expect("temp dir");
    let error = JsonlJournal::open(&temp.path().join(".."))
        .err()
        .expect("no file name");
    assert!(matches!(error, JournalError::InvalidPath(_)), "{error}");
}

#[test]
fn swapped_parents_never_place_a_journal_in_an_unsafe_tree() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let temp = common::private_tempdir().expect("temp dir");
    let pivot = temp.path().join("pivot");
    let alternate = temp.path().join("alternate");
    let relative = (0..32).fold(PathBuf::new(), |path, level| path.join(format!("l{level}")));
    for (root, marker, mode) in [(&pivot, "safe", 0o700), (&alternate, "unsafe", 0o777)] {
        // Every directory gets an explicit mode; only the leaf differs.
        common::dir_all_with_mode(&root.join(&relative), 0o700);
        std::fs::write(root.join("marker"), marker).expect("marker");
        set_mode(&root.join(&relative), mode);
    }

    // Atomically exchange the two trees as fast as possible.
    let stop = Arc::new(AtomicBool::new(false));
    let swapper = {
        let (stop, left, right) = (stop.clone(), pivot.clone(), alternate.clone());
        std::thread::spawn(move || {
            let mut swaps = 0u64;
            while !stop.load(Ordering::Relaxed) {
                rustix::fs::renameat_with(
                    rustix::fs::CWD,
                    &left,
                    rustix::fs::CWD,
                    &right,
                    rustix::fs::RenameFlags::EXCHANGE,
                )
                .expect("exchange trees");
                swaps += 1;
            }
            swaps
        })
    };
    let mut accepted = Vec::new();
    for index in 0..3000 {
        let name = format!("journal-{index}.jsonl");
        if JsonlJournal::open(&pivot.join(&relative).join(&name)).is_ok() {
            accepted.push(name);
        }
    }
    stop.store(true, Ordering::Relaxed);
    let swaps = swapper.join().expect("swapper");

    let unsafe_root = [&pivot, &alternate]
        .into_iter()
        .find(|root| std::fs::read_to_string(root.join("marker")).expect("marker") == "unsafe")
        .expect("unsafe tree");
    let safe_root = if unsafe_root == &pivot {
        &alternate
    } else {
        &pivot
    };
    assert!(swaps > 0, "the trees were never swapped");
    assert!(!accepted.is_empty(), "no journal was ever accepted");
    assert!(
        accepted.len() < 3000,
        "no open ever saw the unsafe tree, so the race was not exercised"
    );
    for name in &accepted {
        assert!(
            !unsafe_root.join(&relative).join(name).exists(),
            "{name} was accepted inside the world-writable tree"
        );
        assert!(safe_root.join(&relative).join(name).is_file(), "{name}");
    }
}

#[test]
fn journal_sources_must_be_regular_files_reached_without_links() {
    let temp = common::private_tempdir().expect("temp dir");
    let fifo = temp.path().join("fifo.jsonl");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(status.success());
    // Without O_NONBLOCK this open would block forever with no writer.
    let started = std::time::Instant::now();
    let error = read_journal(&fifo).expect_err("a FIFO is not a journal");
    assert!(matches!(error, JournalError::NotRegularFile), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1));

    let real = temp.path().join("real.jsonl");
    std::fs::write(&real, b"{}\n").expect("write");
    let link = temp.path().join("link.jsonl");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    let error = read_journal(&link).expect_err("a symlinked source is refused");
    assert!(matches!(error, JournalError::NotRegularFile), "{error}");
}

/// A recorded tool turn followed by a final answer.
async fn record_tool_turn(config: HarnessConfig) -> Vec<JournalRecord> {
    let responses = [
        ModelResponse::tool_use([ToolCall::new("call", "workspace_diff", json!({}))]),
        ModelResponse::final_text("done"),
    ];
    let mut recording = Harness::new(
        FakeApi::default(),
        ScriptedProvider::new(responses),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let _ = recording
        .run(&SessionTask::demo(), CancellationToken::new())
        .await;
    recording.journal().records.clone()
}

fn forge_tool_records(records: &mut [JournalRecord], forge: impl Fn(&mut ToolCall, &mut Value)) {
    for record in records {
        if let JournalEvent::ToolExecuted { call, result, .. } = &mut record.event {
            forge(call, result);
        }
    }
}

#[tokio::test]
async fn a_tool_record_that_differs_from_the_model_call_is_refused_everywhere() {
    let mut records = record_tool_turn(short_lifecycle_config()).await;
    forge_tool_records(&mut records, |call, result| {
        call.tool = "exec".to_string();
        call.arguments = json!({"argv": ["id"]});
        *result = json!({"ok": true, "stdout": "fabricated"});
    });

    // The writer refuses to record it.
    let mut writer = MemoryJournal::default();
    let refused = records
        .iter()
        .try_for_each(|record| writer.append(&record.session_id, record.event.clone()));
    assert!(
        matches!(refused, Err(JournalError::Grammar { .. })),
        "{refused:?}"
    );

    // A rechained file fails verification.
    let temp = common::private_tempdir().expect("temp dir");
    let path = temp.path().join("forged.jsonl");
    let session = records[0].session_id.clone();
    let lines = common::chained_lines(
        &session,
        records.iter().map(|record| record.event.clone()).collect(),
    );
    common::write_journal_lines(&path, &lines);
    assert!(
        matches!(read_journal(&path), Err(JournalError::Grammar { .. })),
        "the forged call verifies"
    );

    // Replay refuses records handed to it directly.
    let loaded = LoadedJournal {
        records,
        complete: true,
    };
    assert!(ReplayProvider::from_journal(&loaded).is_err());
}

#[tokio::test]
async fn a_forged_result_in_the_last_tool_turn_diverges_at_that_tool() {
    // With one step, the tool turn is the last turn: no later request
    // carries its result, so only replay can check it.
    let config = HarnessConfig {
        max_steps: 1,
        ..short_lifecycle_config()
    };
    let mut records = record_tool_turn(config.clone()).await;
    forge_tool_records(&mut records, |_, result| {
        *result = json!({"ok": true, "added": ["fabricated.txt"], "removed": []});
    });
    let loaded = LoadedJournal {
        records,
        complete: true,
    };
    let provider = ReplayProvider::from_journal(&loaded).expect("the grammar cannot see it");
    let task = provider.task();
    let api = FakeApi::default();
    let observer = api.clone();
    let mut replay = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );

    let error = replay
        .run(&task, CancellationToken::new())
        .await
        .expect_err("the replayed tool result differs");

    assert!(
        matches!(
            &error,
            HarnessError::Provider(provider) if provider.kind == ProviderErrorKind::ReplayDiverged
        ),
        "{error}"
    );
    assert_eq!(replay.provider().diverged_at(), Some(1));
    assert!(!replay.provider().exhausted());
    assert_eq!(observer.state().deleted.len(), 1);
}
