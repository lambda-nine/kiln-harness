//! Recovery from responses cut off at their output-token limit: the engine
//! discards them within a per-session allowance, the journal grammar mirrors
//! it, and replay reproduces it.
mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{FakeApi, chained_lines, short_lifecycle_config, write_journal_lines};
use kiln_harness::audit::{AuditEvent, AuditRecord, MemoryAudit};
use kiln_harness::engine::{
    Harness, HarnessConfig, HarnessError, SessionOutcome, SessionTask, truncation_note,
};
use kiln_harness::journal::{
    JournalError, JournalEvent, JournalSink, LoadedJournal, MemoryJournal, RecordedCheck,
    base64_encode, read_journal, request_sha256,
};
use kiln_harness::provider::{
    Message, ModelRequest, ModelResponse, Provider, ProviderContent, ProviderError, ProviderInfo,
    ScriptedProvider, StopReason, ToolCall, Usage,
};
use kiln_harness::replay::ReplayProvider;
use kiln_harness::tools::sanitize_call;
use serde_json::json;
use tokio_util::sync::CancellationToken;

const THINKING: &str = "TRUNCATED-THINKING-never-echoed";
const ARGUMENT: &str = "TRUNCATED-ARGUMENT-never-run";

fn task() -> SessionTask {
    SessionTask::demo()
}

/// A response that spent its whole allowance thinking: no text, no calls,
/// and an opaque thinking block that must never be echoed back.
fn thinking_only(output_tokens: u64) -> ModelResponse {
    ModelResponse {
        text: None,
        tool_calls: Vec::new(),
        stop_reason: StopReason::MaxTokens,
        usage: Usage {
            input_tokens: 100,
            output_tokens,
            ..Usage::default()
        },
        provider_content: Some(ProviderContent {
            provider: "anthropic".to_string(),
            model: Some("claude-haiku-5-5".to_string()),
            blocks: json!([{"type": "thinking", "thinking": THINKING, "signature": "c2ln"}]),
            stop_details: None,
        }),
    }
}

/// A full allowance under the default per-request cap.
fn truncated() -> ModelResponse {
    thinking_only(u64::from(
        HarnessConfig::default().max_output_tokens_per_request,
    ))
}

fn truncated_write() -> ModelResponse {
    ModelResponse {
        stop_reason: StopReason::MaxTokens,
        ..ModelResponse::tool_use([ToolCall::new(
            "w",
            "write_file",
            json!({"path": "cut.txt", "content": ARGUMENT}),
        )])
    }
}

fn truncated_text() -> ModelResponse {
    ModelResponse {
        stop_reason: StopReason::MaxTokens,
        ..ModelResponse::final_text("half an ans")
    }
}

fn diff(id: &str) -> ModelResponse {
    ModelResponse::tool_use([ToolCall::new(id, "workspace_diff", json!({}))])
}

fn note() -> Message {
    Message::User {
        text: truncation_note(HarnessConfig::default().max_output_tokens_per_request),
    }
}

fn prompt() -> Message {
    Message::User {
        text: task().prompt,
    }
}

struct Recording {
    inner: ScriptedProvider,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl Provider for Recording {
    fn info(&self) -> ProviderInfo {
        self.inner.info()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.seen
            .lock()
            .expect("seen")
            .push(request.messages.to_vec());
        self.inner.complete(request).await
    }
}

struct Run {
    result: Result<SessionOutcome, HarnessError>,
    /// The messages of every request, in order.
    seen: Vec<Vec<Message>>,
    audit: Vec<AuditRecord>,
    events: Vec<JournalEvent>,
    /// Execs kilnd saw, the workspace baseline included.
    execs: usize,
}

impl Run {
    fn ended(&self) -> (&str, u32, u32) {
        match self.events.last() {
            Some(JournalEvent::SessionEnded {
                work_outcome,
                steps,
                truncation_recoveries,
                ..
            }) => (work_outcome, *steps, *truncation_recoveries),
            other => panic!("the journal ends with {other:?}"),
        }
    }

    fn discarded(&self) -> Vec<&AuditEvent> {
        self.audit
            .iter()
            .map(|record| &record.event)
            .filter(|event| {
                matches!(event, AuditEvent::ModelTurn { action, .. } if action == "truncated_turn_discarded")
            })
            .collect()
    }
}

async fn run(responses: Vec<ModelResponse>, config: HarnessConfig) -> Run {
    let api = FakeApi::default();
    let observer = api.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Recording {
        inner: ScriptedProvider::new(responses),
        seen: seen.clone(),
    };
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let result = harness.run(&task(), CancellationToken::new()).await;
    assert_eq!(observer.state().deleted.len(), 1, "the sandbox is deleted");
    let execs = observer.state().execs.len();
    let events = harness
        .journal()
        .records
        .iter()
        .map(|record| record.event.clone())
        .collect();
    let audit = harness.audit().records.clone();
    let seen = seen.lock().expect("seen").clone();
    Run {
        result,
        seen,
        audit,
        events,
        execs,
    }
}

fn config() -> HarnessConfig {
    short_lifecycle_config()
}

fn with_recoveries(limit: u32) -> HarnessConfig {
    HarnessConfig {
        max_truncation_recoveries: limit,
        ..config()
    }
}

/// Test 1: a thinking-only truncation, then a tool call, then the answer.
async fn thinking_then_tool_then_answer() -> Run {
    run(
        vec![truncated(), diff("d"), ModelResponse::final_text("done")],
        config(),
    )
    .await
}

// ---------------------------------------------------------------------------
// The engine

#[tokio::test]
async fn a_thinking_only_truncation_is_discarded_and_the_session_goes_on() {
    let run = thinking_then_tool_then_answer().await;
    let outcome = run.result.as_ref().expect("the session completes");
    assert_eq!(outcome.final_answer, "done");
    assert_eq!(outcome.truncation_recoveries, 1);
    assert_eq!(outcome.steps, 3, "the recovery used a step");
    assert_eq!(outcome.tool_calls, 1);
    assert_eq!(run.ended(), ("completed", 3, 1));

    assert_eq!(run.seen.len(), 3);
    assert_eq!(run.seen[0], [prompt()]);
    // The second request is the prompt and the note, nothing else.
    assert_eq!(run.seen[1], [prompt(), note()]);
    let second = serde_json::to_string(&run.seen[1]).expect("encode");
    assert!(!second.contains(THINKING), "{second}");
    // The note stays in the history.
    assert_eq!(run.seen[2][..2], [prompt(), note()]);
    assert_eq!(run.seen[2].len(), 4);
    assert!(
        !serde_json::to_string(&run.seen[2])
            .expect("encode")
            .contains(THINKING)
    );
}

#[test]
fn the_note_names_the_per_request_limit_without_separators() {
    assert_eq!(
        truncation_note(64_000),
        "Your previous response reached the 64000-token output limit before it finished, so it \
         was discarded and none of its tool calls ran. Keep your reasoning short: make the next \
         tool call now, or give your final answer."
    );
}

#[tokio::test]
async fn a_truncated_tool_turn_runs_nothing_and_the_next_request_carries_the_note() {
    let run = run(
        vec![truncated_write(), ModelResponse::final_text("done")],
        config(),
    )
    .await;
    let outcome = run.result.as_ref().expect("recovered");
    assert_eq!(outcome.truncation_recoveries, 1);
    assert_eq!(outcome.tool_calls, 0);
    assert_eq!(run.execs, 1, "only the workspace baseline ran");
    assert!(
        !run.events
            .iter()
            .any(|event| matches!(event, JournalEvent::ToolExecuted { .. }))
    );
    assert_eq!(run.seen[1], [prompt(), note()]);
    assert!(
        !serde_json::to_string(&run.seen[1])
            .expect("encode")
            .contains(ARGUMENT)
    );
}

#[tokio::test]
async fn truncated_text_is_recovered_never_taken_as_the_answer() {
    let run = run(
        vec![truncated_text(), ModelResponse::final_text("whole answer")],
        config(),
    )
    .await;
    let outcome = run.result.as_ref().expect("recovered");
    assert_eq!(outcome.final_answer, "whole answer");
    assert_eq!(outcome.truncation_recoveries, 1);
    assert_eq!(run.seen[1], [prompt(), note()]);
    assert!(
        !run.audit
            .iter()
            .any(|record| matches!(&record.event, AuditEvent::FinalAnswer { turn: 1, .. }))
    );
}

#[tokio::test]
async fn recoveries_are_bounded_per_session() {
    let three = run(
        vec![truncated(), truncated(), truncated()],
        with_recoveries(2),
    )
    .await;
    assert!(
        matches!(three.result, Err(HarnessError::ModelTruncated)),
        "{:?}",
        three.result.err()
    );
    assert_eq!(three.ended(), ("failed", 3, 2));
    assert_eq!(three.seen.len(), 3);

    // Not per streak: a good turn between truncations does not reset it.
    let spread = run(
        vec![truncated(), diff("d"), truncated(), truncated()],
        with_recoveries(2),
    )
    .await;
    assert!(matches!(spread.result, Err(HarnessError::ModelTruncated)));
    assert_eq!(spread.ended(), ("failed", 4, 2));

    let off = run(vec![truncated(), diff("d")], with_recoveries(0)).await;
    assert!(matches!(off.result, Err(HarnessError::ModelTruncated)));
    assert_eq!(off.ended(), ("failed", 1, 0));
    assert!(off.discarded().is_empty());
}

#[tokio::test]
async fn budgets_win_over_recovery() {
    let cap = HarnessConfig::default().max_output_tokens_per_request;
    let over_cap = run(vec![thinking_only(u64::from(cap) + 1), diff("d")], config()).await;
    assert!(
        matches!(over_cap.result, Err(HarnessError::TokenBudget)),
        "{:?}",
        over_cap.result.err()
    );
    assert_eq!(over_cap.ended(), ("failed", 1, 0));
    assert!(over_cap.discarded().is_empty());

    // The truncated output counts toward the session's budgets too.
    let over_total = run(
        vec![truncated(), diff("d")],
        HarnessConfig {
            max_total_output_tokens: u64::from(cap) - 1,
            ..config()
        },
    )
    .await;
    assert!(matches!(over_total.result, Err(HarnessError::TokenBudget)));
    let over_bytes = run(
        vec![truncated_write(), diff("d")],
        HarnessConfig {
            max_total_model_output_bytes: 8,
            ..config()
        },
    )
    .await;
    assert!(matches!(
        over_bytes.result,
        Err(HarnessError::OutputBytesBudget(8))
    ));
    assert_eq!(over_bytes.ended(), ("failed", 1, 0));
}

#[tokio::test]
async fn an_exceeded_context_window_is_never_recovered() {
    for (name, response) in [
        (
            "without tool calls",
            ModelResponse {
                stop_reason: StopReason::ContextExceeded,
                ..ModelResponse::final_text("half")
            },
        ),
        (
            "with tool calls",
            ModelResponse {
                stop_reason: StopReason::ContextExceeded,
                ..truncated_write()
            },
        ),
    ] {
        let run = run(vec![response, diff("d")], with_recoveries(16)).await;
        assert!(
            matches!(run.result, Err(HarnessError::ContextExceeded)),
            "{name}: {:?}",
            run.result.err()
        );
        assert_eq!(
            run.result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .as_deref(),
            Some("model context window was exceeded")
        );
        assert_eq!(run.ended(), ("failed", 1, 0), "{name}");
        assert_eq!(run.execs, 1, "{name}: no tool ran");
        assert!(run.discarded().is_empty(), "{name}");
    }
}

#[tokio::test]
async fn a_recovery_on_the_last_step_ends_with_the_step_budget() {
    let last = run(
        vec![diff("d"), truncated(), ModelResponse::final_text("late")],
        HarnessConfig {
            max_steps: 2,
            ..config()
        },
    )
    .await;
    assert!(
        matches!(last.result, Err(HarnessError::StepBudget(2))),
        "{:?}",
        last.result.err()
    );
    assert_eq!(last.ended(), ("failed", 2, 1));
    assert_eq!(last.seen.len(), 2);
    assert_eq!(last.discarded().len(), 1);
}

#[tokio::test]
async fn each_recovery_is_audited_without_content() {
    let run = run(
        vec![
            truncated(),
            truncated_write(),
            ModelResponse::final_text("done"),
        ],
        config(),
    )
    .await;
    assert_eq!(
        run.result
            .as_ref()
            .expect("recovered")
            .truncation_recoveries,
        2
    );
    let discarded = run.discarded();
    assert_eq!(discarded.len(), 2);
    for (event, expected_turn) in discarded.iter().zip([1, 2]) {
        assert!(
            matches!(
                event,
                AuditEvent::ModelTurn {
                    turn,
                    audit_call_id: None,
                    tool: None,
                    arguments: None,
                    ..
                } if *turn == expected_turn
            ),
            "{event:?}"
        );
    }
    // Nothing of the discarded turns is audited as a call or an answer.
    assert!(!run.audit.iter().any(|record| matches!(
        &record.event,
        AuditEvent::ModelTurn { turn: 1 | 2, action, .. } if action != "truncated_turn_discarded"
    )));
    let audit = serde_json::to_string(&run.audit).expect("encode");
    assert!(!audit.contains(THINKING) && !audit.contains(ARGUMENT));
    assert!(!audit.contains("cut.txt"));
}

// ---------------------------------------------------------------------------
// The journal grammar

const SESSION: &str = "hs-truncation";

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

fn assert_accepted(name: &str, events: Vec<JournalEvent>) {
    write_all(&events).unwrap_or_else(|error| panic!("{name}: writer refused: {error}"));
    let loaded = read_all(events).unwrap_or_else(|error| panic!("{name}: reader refused: {error}"));
    ReplayProvider::from_journal(&loaded)
        .unwrap_or_else(|error| panic!("{name}: replay refused: {error}"));
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
        ReplayProvider::from_journal(&loaded).is_err(),
        "{name}: replay accepted it"
    );
}

/// The records up to and including the first response, as the engine
/// wrote them.
fn through_first_response(events: &[JournalEvent]) -> Vec<JournalEvent> {
    let at = events
        .iter()
        .position(|event| matches!(event, JournalEvent::ModelResponded { .. }))
        .expect("a response");
    events[..=at].to_vec()
}

/// A `session_ended` with the given outcome, answer and recovery count
/// that otherwise agrees with `events`.
fn ended_after(
    events: &[JournalEvent],
    work_outcome: &str,
    final_answer: Option<&str>,
    truncation_recoveries: u32,
) -> JournalEvent {
    let mut steps = 0;
    let mut tool_calls = 0;
    let mut usage = Usage::default();
    for event in events {
        match event {
            JournalEvent::ModelRequested { turn, .. } => steps = *turn,
            JournalEvent::ModelResponded { response, .. } => usage = usage + response.usage,
            JournalEvent::ToolExecuted { .. } => tool_calls += 1,
            _ => {}
        }
    }
    JournalEvent::SessionEnded {
        outcome: work_outcome.to_string(),
        work_outcome: work_outcome.to_string(),
        final_answer: final_answer.map(str::to_string),
        steps,
        tool_calls,
        usage,
        sandbox_deleted: true,
        truncation_recoveries,
    }
}

fn request_with(turn: u32, messages: &[Message], max_output_tokens: u32) -> String {
    // A live session streams its model's commands, and its `exec`
    // description says so.
    let mut tools = kiln_harness::tools::ToolRegistry::new(HarnessConfig::default().tool_limits)
        .definitions()
        .to_vec();
    tools[0]
        .description
        .push_str(kiln_harness::tools::EXEC_STREAM_DESCRIPTION);
    request_sha256(&ModelRequest {
        session_id: "",
        turn,
        system: &task().system,
        tools: &tools,
        messages,
        max_output_tokens,
    })
}

#[tokio::test]
async fn engine_journals_with_recoveries_verify() {
    let journals = [
        (
            "thinking, tool, answer",
            thinking_then_tool_then_answer().await,
        ),
        (
            "truncated tool turn",
            run(
                vec![truncated_write(), ModelResponse::final_text("done")],
                config(),
            )
            .await,
        ),
        (
            "recoveries used up",
            run(
                vec![truncated(), truncated(), truncated()],
                with_recoveries(2),
            )
            .await,
        ),
        (
            "recovery on the last step",
            run(
                vec![diff("d"), truncated()],
                HarnessConfig {
                    max_steps: 2,
                    ..config()
                },
            )
            .await,
        ),
    ];
    for (name, run) in journals {
        assert_accepted(name, run.events);
    }
}

#[tokio::test]
async fn a_discarded_turn_runs_no_tool_and_ends_no_session_successfully() {
    let run = run(
        vec![truncated_write(), ModelResponse::final_text("done")],
        config(),
    )
    .await;
    let mut tool_after = through_first_response(&run.events);
    let JournalEvent::SessionStarted { tools, limits, .. } = &tool_after[0] else {
        panic!("session start");
    };
    let call = sanitize_call(&truncated_write().tool_calls[0], tools, limits);
    tool_after.push(JournalEvent::ToolExecuted {
        turn: 1,
        index: 1,
        audit_call_id: "call-1-1".to_string(),
        call,
        ok: true,
        result: json!({"ok": true}),
        execution_ids: Vec::new(),
    });
    let end = ended_after(&tool_after, "failed", None, 1);
    tool_after.push(end);
    assert_rejected_because(
        "a tool after a discarded turn",
        tool_after,
        "tool_executed after a response the engine discards",
    );

    let text = self::run(
        vec![truncated_text(), ModelResponse::final_text("done")],
        config(),
    )
    .await;
    let mut completed = through_first_response(&text.events);
    completed.push(ended_after(&completed, "completed", Some("half an ans"), 1));
    assert_rejected_because(
        "completed by the discarded text",
        completed,
        "completed after a response the engine discards",
    );
    let mut claimed = through_first_response(&text.events);
    claimed.push(ended_after(&claimed, "failed", Some("half an ans"), 1));
    assert_rejected_because(
        "a final answer from the discarded turn",
        claimed,
        "final_answer without completed work",
    );

    // A recorded check may not run after a discarded turn either.
    let mut checked = through_first_response(&text.events);
    let JournalEvent::SessionStarted { check, .. } = &mut checked[0] else {
        panic!("session start");
    };
    *check = Some(RecordedCheck {
        script_b64: base64_encode(b"exit 0\n"),
        timeout_ms: 5_000,
    });
    checked.push(JournalEvent::CheckCompleted {
        outcome: kiln_harness::engine::CheckOutcome::Passed,
        execution_id: None,
        stdout: String::new(),
        stderr: String::new(),
    });
    checked.push(ended_after(&checked, "failed", None, 1));
    assert_rejected_because(
        "a check after a discarded turn",
        checked,
        "check_completed needs an accepted final answer",
    );

    // The failure endings the engine can reach after a recovery verify,
    // as after a tool turn.
    for work_outcome in ["canceled", "deadline", "failed"] {
        let mut ending = through_first_response(&text.events);
        ending.push(ended_after(&ending, work_outcome, None, 1));
        assert_accepted(work_outcome, ending);
    }
}

#[tokio::test]
async fn the_request_after_a_discarded_turn_must_carry_the_note() {
    let run = thinking_then_tool_then_answer().await;
    let at = run
        .events
        .iter()
        .position(|event| matches!(event, JournalEvent::ModelRequested { turn: 2, .. }))
        .expect("turn 2");
    let JournalEvent::ModelRequested {
        request_sha256: recorded,
        message_count,
        ..
    } = run.events[at].clone()
    else {
        panic!("turn 2");
    };
    assert_eq!(message_count, 2);
    let cap = HarnessConfig::default().max_output_tokens_per_request;
    assert_eq!(recorded, request_with(2, &[prompt(), note()], cap));

    let forged = |message_count: usize, request_sha256: String| {
        let mut events = run.events.clone();
        events[at] = JournalEvent::ModelRequested {
            turn: 2,
            request_sha256,
            message_count,
        };
        events
    };
    // The history without the note, as if the turn had never happened.
    assert_rejected_because(
        "no note",
        forged(1, request_with(2, &[prompt()], cap)),
        "message_count differs from the rebuilt history",
    );
    // The discarded turn kept, as if it were a tool turn.
    assert_rejected_because(
        "the discarded turn kept",
        forged(3, recorded.clone()),
        "message_count differs from the rebuilt history",
    );
    // The right count with another note.
    let other = Message::User {
        text: truncation_note(cap + 1),
    };
    assert_rejected_because(
        "another note",
        forged(2, request_with(2, &[prompt(), other], cap)),
        "hash differs from the rebuilt request",
    );

    // A refused request leaves the grammar exactly as it was: the real one
    // and everything after it still follow.
    let mut journal = MemoryJournal::default();
    for event in &run.events[..at] {
        journal.append(SESSION, event.clone()).expect("prefix");
    }
    for bad in [
        forged(2, request_with(2, &[prompt(), prompt()], cap))[at].clone(),
        forged(1, request_with(2, &[prompt()], cap))[at].clone(),
    ] {
        assert!(matches!(
            journal.append(SESSION, bad),
            Err(JournalError::Grammar { .. })
        ));
    }
    for event in &run.events[at..] {
        journal
            .append(SESSION, event.clone())
            .expect("the real records still follow");
    }
}

#[tokio::test]
async fn recoveries_are_bounded_and_counted_by_the_grammar() {
    let run = thinking_then_tool_then_answer().await;
    // Recorded without recoveries, the same records go beyond the limit.
    let mut over = run.events.clone();
    let JournalEvent::SessionStarted { limits, .. } = &mut over[0] else {
        panic!("session start");
    };
    limits.max_truncation_recoveries = 0;
    assert_rejected_because(
        "a recovery under a limit of 0",
        over,
        "model_requested after a response the engine refuses",
    );

    // A fourth request after the third truncation under a limit of 2.
    let used_up = self::run(
        vec![truncated(), truncated(), truncated()],
        with_recoveries(2),
    )
    .await;
    let mut beyond = used_up.events.clone();
    beyond.pop();
    let history = [prompt(), note(), note(), note()];
    let cap = HarnessConfig::default().max_output_tokens_per_request;
    beyond.push(JournalEvent::ModelRequested {
        turn: 4,
        request_sha256: request_with(4, &history, cap),
        message_count: history.len(),
    });
    assert_rejected_because(
        "a third recovery under a limit of 2",
        beyond,
        "model_requested after a response the engine refuses",
    );

    for wrong in [0, 2] {
        let mut events = run.events.clone();
        let Some(JournalEvent::SessionEnded {
            truncation_recoveries,
            ..
        }) = events.last_mut()
        else {
            panic!("end");
        };
        *truncation_recoveries = wrong;
        assert_rejected_because(
            "a wrong recovery count",
            events,
            "truncation_recoveries differs",
        );
    }

    // An exceeded context is refused, never recovered.
    let context = self::run(
        vec![ModelResponse {
            stop_reason: StopReason::ContextExceeded,
            ..ModelResponse::final_text("half")
        }],
        with_recoveries(16),
    )
    .await;
    let mut continued = through_first_response(&context.events);
    continued.push(JournalEvent::ModelRequested {
        turn: 2,
        request_sha256: request_with(2, &[prompt(), note()], cap),
        message_count: 2,
    });
    assert_rejected_because(
        "a request after an exceeded context",
        continued,
        "model_requested after a response the engine refuses",
    );
}

#[tokio::test]
async fn journals_from_before_recovery_keep_their_meaning() {
    // Recorded with recoveries off, the limit and the count are absent, as
    // in every journal written before they existed.
    let old = run(vec![truncated(), diff("d")], with_recoveries(0)).await;
    let lines = chained_lines(SESSION, old.events.clone());
    assert!(
        lines
            .iter()
            .all(|line| !line.contains("truncation_recoveries"))
    );
    assert_accepted("an old truncated session", old.events.clone());

    // In such a journal a request after a truncated response is refused.
    let mut continued = old.events.clone();
    continued.pop();
    let cap = HarnessConfig::default().max_output_tokens_per_request;
    continued.push(JournalEvent::ModelRequested {
        turn: 2,
        request_sha256: request_with(2, &[prompt(), note()], cap),
        message_count: 2,
    });
    let lines = chained_lines(SESSION, continued.clone());
    assert!(
        lines
            .iter()
            .all(|line| !line.contains("truncation_recoveries"))
    );
    assert_rejected_because(
        "an old journal continued after truncation",
        continued,
        "model_requested after a response the engine refuses",
    );

    // Old journals checked in as fixtures still verify and replay.
    for fixture in ["journal-demo-golden.jsonl", "openai-compat-p2.jsonl"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture);
        let text = std::fs::read_to_string(&path).expect("fixture");
        assert!(!text.contains("truncation_recoveries"), "{fixture}");
        let loaded = read_journal(&path).unwrap_or_else(|error| panic!("{fixture}: {error}"));
        let replay = ReplayProvider::from_journal(&loaded).expect("replays");
        assert_eq!(replay.limits().max_truncation_recoveries, 0, "{fixture}");
    }

    // A journal with recoveries records its limit and count.
    let new = thinking_then_tool_then_answer().await;
    let lines = chained_lines(SESSION, new.events);
    assert!(lines[0].contains("\"max_truncation_recoveries\":2"));
    assert!(
        lines
            .last()
            .is_some_and(|line| line.contains("\"truncation_recoveries\":1"))
    );
}

// ---------------------------------------------------------------------------
// Replay

#[tokio::test]
async fn a_recorded_recovery_replays_as_matched() {
    let recorded = thinking_then_tool_then_answer().await;
    let expected = recorded.result.as_ref().expect("recorded").clone();
    let loaded = read_all(recorded.events).expect("verifies");
    let provider = ReplayProvider::from_journal(&loaded).expect("replayable");
    assert_eq!(provider.limits().max_truncation_recoveries, 2);
    // The recorded limit drives the replay, whatever the base says.
    let config = provider.replay_config(with_recoveries(0));
    assert_eq!(config.max_truncation_recoveries, 2);
    let task = provider.task();
    let mut harness = Harness::new(
        FakeApi::default(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let replayed = harness
        .run(&task, CancellationToken::new())
        .await
        .expect("the replay completes");
    assert_eq!(replayed.final_answer, expected.final_answer);
    assert_eq!(replayed.truncation_recoveries, 1);
    assert_eq!(replayed.steps, expected.steps);
    let replay = harness.provider();
    assert_eq!(replay.diverged_at(), None);
    assert!(replay.exhausted());
    assert!(!replay.check_diverged());
}

// ---------------------------------------------------------------------------
// Audit failures and impossible endings

/// Fails every append of one audit event kind, and nothing else.
struct FailingAudit {
    fails: fn(&AuditEvent) -> bool,
}

impl kiln_harness::audit::AuditSink for FailingAudit {
    fn append(
        &mut self,
        _session_id: &str,
        _sandbox_id: Option<&str>,
        event: AuditEvent,
    ) -> Result<(), kiln_harness::audit::AuditError> {
        if (self.fails)(&event) {
            return Err(kiln_harness::audit::AuditError::Io(std::io::Error::other(
                "injected audit failure",
            )));
        }
        Ok(())
    }
}

/// A name, the audit appends that fail, the response, and the recoveries
/// `session_ended` must record.
type AuditFailureCase = (&'static str, fn(&AuditEvent) -> bool, ModelResponse, u32);

#[tokio::test]
async fn an_audit_failure_after_a_recoverable_response_still_ends_the_journal() {
    let cap = u64::from(HarnessConfig::default().max_output_tokens_per_request);
    let cases: [AuditFailureCase; 3] = [
        (
            "the model_responded append",
            |event| matches!(event, AuditEvent::ModelResponded { .. }),
            truncated_text(),
            1,
        ),
        (
            "the truncated_turn_discarded append",
            |event| matches!(event, AuditEvent::ModelTurn { action, .. } if action == "truncated_turn_discarded"),
            truncated(),
            1,
        ),
        (
            "model_responded over a budget, never a recovery",
            |event| matches!(event, AuditEvent::ModelResponded { .. }),
            thinking_only(cap + 1),
            0,
        ),
    ];
    for (name, fails, response, recoveries) in cases {
        let api = FakeApi::default();
        let observer = api.clone();
        let mut harness = Harness::new(
            api,
            ScriptedProvider::new([response, ModelResponse::final_text("done")]),
            FailingAudit { fails },
            MemoryJournal::default(),
            config(),
        );
        let error = harness
            .run(&task(), CancellationToken::new())
            .await
            .expect_err(name);
        assert!(
            matches!(&error, HarnessError::Audit(_)),
            "{name}: the audit error, not a journal one: {error}"
        );
        assert_eq!(observer.state().deleted.len(), 1, "{name}: cleanup");
        let events = harness
            .journal()
            .records
            .iter()
            .map(|record| record.event.clone())
            .collect::<Vec<_>>();
        assert!(
            matches!(
                events.last(),
                Some(JournalEvent::SessionEnded {
                    work_outcome,
                    sandbox_deleted: true,
                    truncation_recoveries,
                    ..
                }) if work_outcome == "failed" && *truncation_recoveries == recoveries
            ),
            "{name}: {:?}",
            events.last()
        );
        assert_accepted(name, events);
    }
}

#[tokio::test]
async fn a_last_step_recovery_ends_only_as_failed_work() {
    let last = run(
        vec![truncated_text()],
        HarnessConfig {
            max_steps: 1,
            ..config()
        },
    )
    .await;
    assert!(matches!(last.result, Err(HarnessError::StepBudget(1))));
    assert_accepted("the engine's ending", last.events.clone());
    let ending = |outcome: &str, work_outcome: &str, sandbox_deleted: bool| {
        let mut events = last.events.clone();
        let Some(JournalEvent::SessionEnded {
            outcome: recorded_outcome,
            work_outcome: recorded_work,
            sandbox_deleted: recorded_deleted,
            ..
        }) = events.last_mut()
        else {
            panic!("end");
        };
        *recorded_outcome = outcome.to_string();
        *recorded_work = work_outcome.to_string();
        *recorded_deleted = sandbox_deleted;
        events
    };
    // Unconfirmed cleanup still wraps failed work.
    assert_accepted("cleanup failed", ending("cleanup_failed", "failed", false));
    for impossible in ["canceled", "deadline"] {
        assert_rejected_because(
            impossible,
            ending(impossible, impossible, true),
            "a recovery on the last step ends only as failed work",
        );
        assert_rejected_because(
            impossible,
            ending("cleanup_failed", impossible, false),
            "a recovery on the last step ends only as failed work",
        );
    }
    assert_rejected_because(
        "create_failed",
        ending("create_failed", "create_failed", true),
        "create_failed after sandbox_ready or a model request",
    );

    // Before the last step, cancellation and the deadline remain possible.
    let earlier = run(vec![truncated_text()], config()).await;
    let mut canceled = through_first_response(&earlier.events);
    canceled.push(ended_after(&canceled, "canceled", None, 1));
    assert_accepted("canceled before the last step", canceled);
}

#[tokio::test]
async fn create_failed_needs_no_journaled_sandbox_ready() {
    // What the engine writes when create is rejected verifies.
    let api = FakeApi::default();
    api.state().create_rejection = Some(kiln_harness::api::ApiError::from(tonic::Status::new(
        tonic::Code::InvalidArgument,
        "rejected",
    )));
    let mut harness = Harness::new(
        api,
        ScriptedProvider::new([]),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config(),
    );
    harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("create is rejected");
    let rejected = harness
        .journal()
        .records
        .iter()
        .map(|record| record.event.clone())
        .collect::<Vec<_>>();
    assert!(matches!(
        rejected.last(),
        Some(JournalEvent::SessionEnded { work_outcome, .. }) if work_outcome == "create_failed"
    ));
    assert_accepted("a rejected create", rejected.clone());

    // After sandbox_ready, with or without a model request, it cannot be.
    let ran = thinking_then_tool_then_answer().await;
    let ready = ran
        .events
        .iter()
        .position(|event| matches!(event, JournalEvent::SandboxReady { .. }))
        .expect("ready");
    let mut after_ready = ran.events[..=ready].to_vec();
    after_ready.push(ended_after(&after_ready, "create_failed", None, 0));
    assert_rejected_because(
        "create_failed after sandbox_ready",
        after_ready,
        "create_failed after sandbox_ready or a model request",
    );
    let mut after_request = ran.events[..=ready + 1].to_vec();
    after_request.push(ended_after(&after_request, "create_failed", None, 0));
    assert_rejected_because(
        "create_failed after a request",
        after_request,
        "create_failed after sandbox_ready or a model request",
    );
}
