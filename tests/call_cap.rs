//! Calls past `max_tool_calls_per_turn` in one response: answered with a
//! tool error, up to `max_refused_calls_per_turn`, and recorded so the
//! journal grammar and replay reproduce them.
mod common;

use async_trait::async_trait;
use common::{FakeApi, chained_lines, short_lifecycle_config, write_journal_lines};
use kiln_harness::audit::{AuditEvent, MemoryAudit};
use kiln_harness::engine::{
    Harness, HarnessConfig, HarnessError, SessionOutcome, SessionTask, turn_cap_refusal,
};
use kiln_harness::journal::{
    JournalError, JournalEvent, JournalSink, LoadedJournal, MemoryJournal, SessionLimits,
    read_journal,
};
use kiln_harness::provider::{
    Message, ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind, ProviderInfo,
    ScriptedProvider, ToolCall,
};
use kiln_harness::replay::{ReplayError, ReplayProvider};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const SESSION: &str = "hs-call-cap";

fn diff(id: &str) -> ToolCall {
    ToolCall::new(id, "workspace_diff", json!({}))
}

fn diffs(count: usize) -> Vec<ToolCall> {
    (1..=count).map(|n| diff(&format!("c{n}"))).collect()
}

fn config(max_tool_calls: u32) -> HarnessConfig {
    HarnessConfig {
        max_tool_calls,
        ..short_lifecycle_config()
    }
}

/// A scripted provider that keeps the tool results each request carried.
struct Capture {
    inner: ScriptedProvider,
    results_seen: Vec<Vec<Value>>,
}

#[async_trait]
impl Provider for Capture {
    fn info(&self) -> ProviderInfo {
        self.inner.info()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        if let Some(Message::ToolResults { results }) = request.messages.last() {
            self.results_seen
                .push(results.iter().map(|result| result.result.clone()).collect());
        }
        self.inner.complete(request).await
    }
}

struct Run {
    result: Result<SessionOutcome, HarnessError>,
    api: FakeApi,
    harness: Harness<FakeApi, Capture, MemoryAudit, MemoryJournal>,
}

impl Run {
    fn audit(&self) -> &MemoryAudit {
        self.harness.audit()
    }

    fn journal(&self) -> &MemoryJournal {
        self.harness.journal()
    }

    fn results_seen(&self) -> &[Vec<Value>] {
        &self.harness.provider().results_seen
    }
}

async fn run(responses: Vec<ModelResponse>, config: HarnessConfig) -> Run {
    let api = FakeApi::default();
    let provider = Capture {
        inner: ScriptedProvider::new(responses),
        results_seen: Vec::new(),
    };
    let mut harness = Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let result = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await;
    Run {
        result,
        api,
        harness,
    }
}

/// `(index, ok, result, execution_ids)` of each journaled tool.
fn executed(journal: &MemoryJournal) -> Vec<(u32, bool, Value, Vec<String>)> {
    journal
        .records
        .iter()
        .filter_map(|record| match &record.event {
            JournalEvent::ToolExecuted {
                index,
                ok,
                result,
                execution_ids,
                ..
            } => Some((*index, *ok, result.clone(), execution_ids.clone())),
            _ => None,
        })
        .collect()
}

fn rejected_call_ids(audit: &MemoryAudit) -> Vec<Option<String>> {
    audit
        .records
        .iter()
        .filter_map(|record| match &record.event {
            AuditEvent::ModelTurn {
                action,
                audit_call_id,
                ..
            } if action == "tool_call_rejected" => Some(audit_call_id.clone()),
            _ => None,
        })
        .collect()
}

fn events(journal: &MemoryJournal) -> Vec<JournalEvent> {
    journal
        .records
        .iter()
        .map(|record| record.event.clone())
        .collect()
}

/// A ten-call turn under the default cap of 8 and allowance of 24, then a
/// final answer.
async fn ten_call_session() -> Run {
    run(
        vec![
            ModelResponse::tool_use(diffs(10)),
            ModelResponse::final_text("done"),
        ],
        config(20),
    )
    .await
}

#[test]
fn the_refusal_is_exact_and_fits_the_result_bound() {
    assert_eq!(
        serde_json::to_string(&turn_cap_refusal(8, 9, 10, 131_072)).expect("json"),
        r#"{"error":"too_many_calls_in_turn","message":"only the first 8 tool calls in a turn run; this was call 9 of 10, so it did not run. Send it again in a later turn.","ok":false}"#
    );
    // The longest message, with the largest numbers the ceilings allow.
    let longest = turn_cap_refusal(64, 128, 128, 131_072);
    let bytes = serde_json::to_vec(&longest).expect("json").len();
    assert!(longest.get("message").is_some());
    assert!(bytes <= 200, "{bytes} bytes");
    assert_eq!(turn_cap_refusal(64, 128, 128, bytes), longest);
    // Below that, only the error, which fits the 64-byte minimum.
    let plain = json!({"ok": false, "error": "too_many_calls_in_turn"});
    assert_eq!(turn_cap_refusal(64, 128, 128, bytes - 1), plain);
    assert_eq!(turn_cap_refusal(8, 9, 10, 64), plain);
    assert!(serde_json::to_vec(&plain).expect("json").len() <= 64);
}

#[tokio::test]
async fn calls_past_the_cap_are_refused_and_the_session_continues() {
    let ten = ten_call_session().await;
    let outcome = ten.result.as_ref().expect("the session completes");
    assert_eq!(outcome.final_answer, "done");
    assert_eq!(outcome.tool_calls, 10, "refused calls count");

    let tools = executed(ten.journal());
    assert_eq!(tools.len(), 10);
    for (index, ok, result, execution_ids) in &tools[..8] {
        assert!(*ok, "call {index} ran");
        assert_eq!(execution_ids.len(), 1, "call {index} ran one exec");
        assert_ne!(result["error"], "too_many_calls_in_turn");
    }
    for (index, ok, result, execution_ids) in &tools[8..] {
        assert!(!*ok);
        assert!(execution_ids.is_empty());
        assert_eq!(*result, turn_cap_refusal(8, *index, 10, 131_072));
        assert_eq!(
            result["message"],
            format!(
                "only the first 8 tool calls in a turn run; this was call {index} of 10, \
                 so it did not run. Send it again in a later turn."
            )
        );
    }
    assert_eq!(
        rejected_call_ids(ten.audit()),
        [Some("call-1-9".to_string()), Some("call-1-10".to_string())]
    );
    // Every call's result reached the model, the refusals included.
    assert_eq!(ten.results_seen().len(), 1);
    assert_eq!(ten.results_seen()[0].len(), 10);
    assert_eq!(ten.results_seen()[0][9], tools[9].2);

    // The refused calls reached kilnd no more than a turn of eight did.
    let eight = run(
        vec![
            ModelResponse::tool_use(diffs(8)),
            ModelResponse::final_text("done"),
        ],
        config(20),
    )
    .await;
    eight.result.as_ref().expect("eight calls complete");
    assert_eq!(ten.api.state().execs.len(), eight.api.state().execs.len());
    assert_eq!(ten.api.state().deleted.len(), 1);
}

#[tokio::test]
async fn a_turn_past_the_cap_and_the_allowance_ends_the_session_before_any_call() {
    for (refused, calls) in [(3, 6), (0, 3)] {
        let over = run(
            vec![ModelResponse::tool_use(diffs(calls))],
            HarnessConfig {
                max_tool_calls_per_turn: 2,
                max_refused_calls_per_turn: refused,
                ..config(20)
            },
        )
        .await;
        assert!(
            matches!(over.result, Err(HarnessError::TurnToolBudget(2))),
            "{refused}: {:?}",
            over.result
        );
        assert_eq!(over.api.state().execs.len(), 1, "only the baseline ran");
        assert_eq!(over.api.state().deleted.len(), 1);
        assert!(executed(over.journal()).is_empty());
        assert_eq!(rejected_call_ids(over.audit()), [None]);
        assert!(
            !over
                .audit()
                .records
                .iter()
                .any(|record| matches!(&record.event, AuditEvent::ToolCompleted { .. }))
        );
    }
    // Exactly at the cap plus the allowance, the turn runs.
    let at = run(
        vec![
            ModelResponse::tool_use(diffs(5)),
            ModelResponse::final_text("done"),
        ],
        HarnessConfig {
            max_tool_calls_per_turn: 2,
            max_refused_calls_per_turn: 3,
            ..config(20)
        },
    )
    .await;
    at.result.as_ref().expect("five calls under 2 + 3 complete");
    assert_eq!(executed(at.journal()).len(), 5);
}

#[tokio::test]
async fn with_no_allowance_a_turn_over_the_cap_is_fatal_as_before() {
    let nine = run(
        vec![ModelResponse::tool_use(diffs(9))],
        HarnessConfig {
            max_refused_calls_per_turn: 0,
            ..config(20)
        },
    )
    .await;
    assert!(matches!(nine.result, Err(HarnessError::TurnToolBudget(8))));
    assert_eq!(nine.api.state().execs.len(), 1, "only the baseline ran");
    assert!(executed(nine.journal()).is_empty());
}

#[tokio::test]
async fn refusals_count_toward_the_session_call_budget() {
    let over = run(
        vec![
            ModelResponse::tool_use(diffs(10)),
            ModelResponse::final_text("done"),
        ],
        config(9),
    )
    .await;
    assert!(matches!(over.result, Err(HarnessError::ToolBudget(9))));
    let tools = executed(over.journal());
    assert_eq!(tools.len(), 9, "the tenth call is over the session budget");
    assert_eq!(tools[8].2, turn_cap_refusal(8, 9, 10, 131_072));
    let recorded = events(over.journal());
    assert!(matches!(
        recorded.last(),
        Some(JournalEvent::SessionEnded { .. })
    ));
    write_all(&recorded).expect("the grammar accepts the journal");
    assert_eq!(
        rejected_call_ids(over.audit()),
        [Some("call-1-9".to_string()), Some("call-1-10".to_string())]
    );
}

#[tokio::test]
async fn the_turn_cap_wins_over_other_host_side_rejections() {
    let mut calls = diffs(8);
    calls.push(ToolCall::new("u9", "not_a_tool", json!({})));
    calls.push(ToolCall::new(
        "big10",
        "exec",
        json!({"argv": ["x".repeat(2000)]}),
    ));
    calls.push(ToolCall::new("u11", "not_a_tool", json!({})));
    let mixed = run(
        vec![
            ModelResponse::tool_use(calls),
            ModelResponse::final_text("done"),
        ],
        HarnessConfig {
            max_model_arguments_bytes: 1024,
            ..config(20)
        },
    )
    .await;
    mixed.result.as_ref().expect("the session completes");
    let tools = executed(mixed.journal());
    for (index, ok, result, execution_ids) in &tools[8..] {
        assert!(!*ok && execution_ids.is_empty());
        assert_eq!(*result, turn_cap_refusal(8, *index, 11, 131_072), "{index}");
    }
    // Under the cap, the same calls get their own errors.
    let mut calls = diffs(1);
    calls.push(ToolCall::new("u2", "not_a_tool", json!({})));
    let under = run(
        vec![
            ModelResponse::tool_use(calls),
            ModelResponse::final_text("done"),
        ],
        config(20),
    )
    .await;
    under.result.as_ref().expect("the session completes");
    assert_eq!(executed(under.journal())[1].2["error"], "unknown_tool");
}

// The journal grammar.

fn write_all(events: &[JournalEvent]) -> Result<(), JournalError> {
    let mut journal = MemoryJournal::default();
    for event in events {
        journal.append(SESSION, event.clone())?;
    }
    Ok(())
}

fn read_all(events: Vec<JournalEvent>) -> Result<LoadedJournal, JournalError> {
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("journal.jsonl");
    write_journal_lines(&path, &chained_lines(SESSION, events));
    read_journal(&path)
}

fn assert_rejected_because(name: &str, events: Vec<JournalEvent>, reason: &str) {
    let written = write_all(&events);
    assert!(
        matches!(&written, Err(JournalError::Grammar { reason: found, .. }) if found.contains(reason)),
        "{name}: writer gave {written:?}, expected {reason:?}"
    );
    let read = read_all(events.clone());
    assert!(
        matches!(&read, Err(JournalError::Grammar { reason: found, .. }) if found.contains(reason)),
        "{name}: reader gave {read:?}, expected {reason:?}"
    );
    let loaded = LoadedJournal {
        complete: true,
        records: chained_lines(SESSION, events)
            .iter()
            .map(|line| serde_json::from_str(line).expect("record"))
            .collect(),
    };
    assert!(
        matches!(
            ReplayProvider::from_journal(&loaded),
            Err(ReplayError::Grammar(_))
        ),
        "{name}: replay accepted it"
    );
}

/// The ten-call session's events with tool record `index` changed.
fn with_tool(
    mut events: Vec<JournalEvent>,
    index: u32,
    change: impl FnOnce(&mut bool, &mut Value, &mut Vec<String>),
) -> Vec<JournalEvent> {
    let record = events
        .iter_mut()
        .find_map(|event| match event {
            JournalEvent::ToolExecuted {
                index: found,
                ok,
                result,
                execution_ids,
                ..
            } if *found == index => Some((ok, result, execution_ids)),
            _ => None,
        })
        .expect("tool record");
    change(record.0, record.1, record.2);
    events
}

#[tokio::test]
async fn the_grammar_accepts_refusals_and_only_the_exact_ones() {
    let ten = ten_call_session().await;
    let recorded = events(ten.journal());
    write_all(&recorded).expect("the writer accepts the engine's journal");
    let loaded = read_all(recorded.clone()).expect("the reader accepts it");
    ReplayProvider::from_journal(&loaded).expect("replayable");

    let reason = "tool_executed beyond max_tool_calls_per_turn must be the turn-cap refusal";
    assert_rejected_because(
        "a refused call recorded as ok",
        with_tool(recorded.clone(), 9, |ok, _, _| *ok = true),
        reason,
    );
    assert_rejected_because(
        "a refused call with an execution",
        with_tool(recorded.clone(), 10, |_, _, ids| {
            ids.push("x-1".to_string());
        }),
        reason,
    );
    assert_rejected_because(
        "a refused call with another result",
        with_tool(recorded.clone(), 9, |_, result, _| {
            *result = turn_cap_refusal(8, 10, 10, 131_072);
        }),
        reason,
    );
    assert_rejected_because(
        "a refused call with the plain refusal that the bound did not need",
        with_tool(recorded.clone(), 9, |_, result, _| {
            *result = json!({"ok": false, "error": "too_many_calls_in_turn"});
        }),
        reason,
    );
    // A call within the cap is never refused for it, in either form.
    let early = "tool_executed within max_tool_calls_per_turn cannot be the turn-cap refusal";
    for (index, refusal) in [
        (8, turn_cap_refusal(8, 8, 10, 131_072)),
        (1, json!({"ok": false, "error": "too_many_calls_in_turn"})),
    ] {
        assert_rejected_because(
            &format!("call {index} recorded as refused"),
            with_tool(recorded.clone(), index, |ok, result, ids| {
                *ok = false;
                ids.clear();
                *result = refusal.clone();
            }),
            early,
        );
    }
    // More calls than the recorded cap and allowance: refused whole.
    for refused in [1, 0] {
        let mut over = recorded.clone();
        let JournalEvent::SessionStarted { limits, .. } = &mut over[0] else {
            panic!("session start");
        };
        limits.max_refused_calls_per_turn = refused;
        assert_rejected_because(
            &format!("ten calls over 8 + {refused}"),
            over,
            "tool_executed after a response the engine refuses",
        );
    }
}

// Recorded limits.

#[test]
fn a_journal_without_the_allowance_loads_with_none_and_stays_byte_identical() {
    let mut limits = HarnessConfig::default().limits();
    assert_eq!(limits.max_refused_calls_per_turn, 24);
    assert!(
        serde_json::to_string(&limits)
            .expect("json")
            .contains(r#""max_refused_calls_per_turn":24"#)
    );
    limits.max_refused_calls_per_turn = 0;
    let old = serde_json::to_string(&limits).expect("json");
    assert!(!old.contains("max_refused_calls_per_turn"), "{old}");
    let loaded: SessionLimits = serde_json::from_str(&old).expect("loads");
    assert_eq!(loaded.max_refused_calls_per_turn, 0);

    // The checked-in journal recorded before the allowance existed.
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/journal-demo-golden.jsonl"
    );
    let golden = std::fs::read_to_string(path).expect("golden");
    assert!(!golden.contains("max_refused_calls_per_turn"));
    let journal = read_journal(std::path::Path::new(path)).expect("golden verifies");
    let provider = ReplayProvider::from_journal(&journal).expect("replayable");
    assert_eq!(provider.limits().max_refused_calls_per_turn, 0);
    let config = provider.replay_config(short_lifecycle_config());
    assert_eq!(config.max_refused_calls_per_turn, 0);
}

#[tokio::test]
async fn recorded_limits_above_the_allowance_ceiling_are_rejected_before_create() {
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        ScriptedProvider::new([ModelResponse::final_text("done")]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        HarnessConfig {
            max_refused_calls_per_turn: kiln_harness::engine::MAX_REFUSED_CALLS_PER_TURN + 1,
            ..short_lifecycle_config()
        },
    );
    let error = harness
        .run(&SessionTask::demo(), CancellationToken::new())
        .await
        .expect_err("over the ceiling");
    assert!(
        matches!(error, HarnessError::InvalidModelCall(_)),
        "{error}"
    );
    assert!(api.state().creates.is_empty());
}

// Replay.

async fn replay(journal: &LoadedJournal, change: impl FnOnce(&mut HarnessConfig)) -> Replayed {
    let provider = ReplayProvider::from_journal(journal).expect("replayable");
    let mut config = provider.replay_config(short_lifecycle_config());
    change(&mut config);
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let result = harness.replay(CancellationToken::new()).await;
    Replayed {
        result,
        harness,
        api,
    }
}

struct Replayed {
    result: Result<SessionOutcome, HarnessError>,
    harness: Harness<FakeApi, ReplayProvider, MemoryAudit, MemoryJournal>,
    api: FakeApi,
}

#[tokio::test]
async fn replay_reproduces_the_refusals_and_notices_a_changed_one() {
    let ten = ten_call_session().await;
    let recorded = events(ten.journal());
    let journal = read_all(recorded.clone()).expect("verifies");

    let replayed = replay(&journal, |_| {}).await;
    let outcome = replayed.result.as_ref().expect("replay reproduces");
    assert_eq!(outcome.final_answer, "done");
    assert_eq!(replayed.harness.provider().diverged_at(), None);
    assert!(replayed.harness.provider().exhausted());
    assert_eq!(
        executed(replayed.harness.journal()),
        executed(ten.journal())
    );
    assert_eq!(
        replayed.api.state().execs.len(),
        ten.api.state().execs.len(),
        "the refused calls ran nothing on replay either"
    );

    // A replay under another cap runs call 9 where the recording refused
    // it, and diverges there.
    let diverged = replay(&journal, |config| config.max_tool_calls_per_turn = 9).await;
    let Err(HarnessError::Provider(error)) = diverged.result else {
        panic!("diverged: {:?}", diverged.result);
    };
    assert_eq!(error.kind, ProviderErrorKind::ReplayDiverged);
    assert!(error.message.contains("tool call 9"), "{}", error.message);
    assert_eq!(diverged.harness.provider().diverged_at(), Some(1));

    // A recording whose refusal was changed is refused before any sandbox.
    let forged = with_tool(recorded, 10, |_, result, _| {
        result["message"] = json!("run it anyway");
    });
    let loaded = LoadedJournal {
        complete: true,
        records: chained_lines(SESSION, forged)
            .iter()
            .map(|line| serde_json::from_str(line).expect("record"))
            .collect(),
    };
    assert!(matches!(
        ReplayProvider::from_journal(&loaded),
        Err(ReplayError::Grammar(_))
    ));
}
