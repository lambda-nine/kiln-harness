//! Recorded context compaction: the engine compacts when the policy says it
//! is due, the journal grammar mirrors every decision, replay answers from
//! the recording, and the Anthropic adapter speaks the on-demand wire form.
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::http::{FakeHttp, Reply};
use common::{FakeApi, chained_lines, short_lifecycle_config, write_journal_lines};
use kiln_harness::audit::{AuditEvent, AuditRecord, MemoryAudit};
use kiln_harness::compaction::{
    COMPACTION_BETA, COMPACTION_INSTRUCTIONS, CompactionPolicy, RESUME_NOTE, instructions_sha256,
};
use kiln_harness::engine::{Harness, HarnessConfig, HarnessError, SessionOutcome, SessionTask};
use kiln_harness::journal::{
    JournalError, JournalEvent, LoadedJournal, MemoryJournal, compaction_request_sha256,
    read_journal,
};
use kiln_harness::provider::{
    CompactionResponse, Message, ModelRequest, ModelResponse, Provider, ProviderContent,
    ProviderError, ProviderErrorKind, ProviderInfo, ScriptedProvider, StopReason, ToolCall,
    ToolDefinition, Usage,
};
use kiln_harness::providers::anthropic::{
    FALLBACK_BETA, compaction_body, parse_compaction, request_body,
};
use kiln_harness::providers::{AnthropicConfig, AnthropicProvider, SecretString};
use kiln_harness::replay::ReplayProvider;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

const SIGNATURE: &str = "EuYBCkQYsummary/sig+nature==";
const SUMMARY: &str = "The task is the demo. demo.txt was diffed; next, answer.";
const THRESHOLD: u64 = 20_000;

// Helpers.

fn task() -> SessionTask {
    SessionTask::demo()
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        ..Usage::default()
    }
}

fn diff(id: &str, used: Usage) -> ModelResponse {
    ModelResponse {
        usage: used,
        ..ModelResponse::tool_use([ToolCall::new(id, "workspace_diff", json!({}))])
    }
}

fn answer(used: Usage) -> ModelResponse {
    ModelResponse {
        usage: used,
        ..ModelResponse::final_text("done")
    }
}

fn summary_content(summary: &str) -> ProviderContent {
    ProviderContent {
        provider: "anthropic".to_string(),
        model: Some("claude-opus-5-5".to_string()),
        blocks: json!([{"type": "compaction", "content": summary, "signature": SIGNATURE}]),
        stop_details: None,
    }
}

#[allow(clippy::unnecessary_wraps)] // a scripted compaction's answer
fn summarized(used: Usage) -> Result<CompactionResponse, ProviderError> {
    Ok(CompactionResponse {
        stop_reason: "compaction".to_string(),
        summary: Some(SUMMARY.to_string()),
        usage: used,
        provider_content: Some(summary_content(SUMMARY)),
    })
}

#[allow(clippy::unnecessary_wraps)] // a scripted compaction's answer
fn no_summary(stop: &str) -> Result<CompactionResponse, ProviderError> {
    Ok(CompactionResponse::no_summary(stop, usage(30_000, 50)))
}

fn compacting(compact_at: u64) -> HarnessConfig {
    HarnessConfig {
        max_steps: 30,
        max_tool_calls: 30,
        compaction: Some(CompactionPolicy::new(compact_at)),
        ..short_lifecycle_config()
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Complete(u32, Vec<Message>),
    Compact(u32, Vec<Message>),
}

/// A scripted provider that records every request it is asked, and can
/// cancel the session from inside a compaction and never answer it.
struct Observed {
    inner: ScriptedProvider,
    seen: Arc<Mutex<Vec<Seen>>>,
    cancel_in_compaction: Option<CancellationToken>,
}

#[async_trait]
impl Provider for Observed {
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
            .push(Seen::Complete(request.turn, request.messages.to_vec()));
        self.inner.complete(request).await
    }

    async fn compact(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        self.seen
            .lock()
            .expect("seen")
            .push(Seen::Compact(request.turn, request.messages.to_vec()));
        if let Some(cancel) = &self.cancel_in_compaction {
            cancel.cancel();
            std::future::pending::<()>().await;
        }
        self.inner.compact(request).await
    }
}

struct Run {
    result: Result<SessionOutcome, HarnessError>,
    seen: Vec<Seen>,
    events: Vec<JournalEvent>,
    lines: Vec<String>,
    audit: Vec<AuditRecord>,
    deleted: usize,
}

impl Run {
    fn names(&self) -> Vec<&'static str> {
        self.events.iter().map(name).collect()
    }

    fn ended(&self) -> (&str, u32, Usage, u32) {
        match self.events.last() {
            Some(JournalEvent::SessionEnded {
                work_outcome,
                steps,
                usage,
                compactions,
                ..
            }) => (work_outcome, *steps, *usage, *compactions),
            other => panic!("the journal ends with {other:?}"),
        }
    }

    /// The journal written to a file and read back, chain and grammar
    /// verified.
    fn verified(&self) -> LoadedJournal {
        let temp = common::private_tempdir().expect("tempdir");
        let path = temp.path().join("run.jsonl");
        write_journal_lines(&path, &self.lines);
        read_journal(&path).expect("the engine's journal verifies")
    }

    fn compacted_audit(&self) -> Vec<(u32, String, usize)> {
        self.audit
            .iter()
            .filter_map(|record| match &record.event {
                AuditEvent::ContextCompacted {
                    turn,
                    outcome,
                    messages_summarized,
                    ..
                } => Some((*turn, outcome.clone(), *messages_summarized)),
                _ => None,
            })
            .collect()
    }
}

fn name(event: &JournalEvent) -> &'static str {
    match event {
        JournalEvent::SessionStarted { .. } => "session_started",
        JournalEvent::SandboxReady { .. } => "sandbox_ready",
        JournalEvent::ModelRequested { .. } => "model_requested",
        JournalEvent::ModelResponded { .. } => "model_responded",
        JournalEvent::ModelFailed { .. } => "model_failed",
        JournalEvent::CompactionRequested { .. } => "compaction_requested",
        JournalEvent::CompactionResponded { .. } => "compaction_responded",
        JournalEvent::CompactionFailed { .. } => "compaction_failed",
        JournalEvent::ToolExecuted { .. } => "tool_executed",
        JournalEvent::CheckCompleted { .. } => "check_completed",
        JournalEvent::SessionEnded { .. } => "session_ended",
    }
}

async fn run_with(
    responses: Vec<ModelResponse>,
    compactions: Vec<Result<CompactionResponse, ProviderError>>,
    config: HarnessConfig,
    cancel_in_compaction: bool,
) -> Run {
    let api = FakeApi::default();
    let observer = api.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let cancel = CancellationToken::new();
    let provider = Observed {
        inner: ScriptedProvider::new(responses).with_compactions(compactions),
        seen: seen.clone(),
        cancel_in_compaction: cancel_in_compaction.then(|| cancel.clone()),
    };
    let mut harness = Harness::new(
        api,
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let result = harness.run(&task(), cancel).await;
    let journal = harness.journal();
    let run = Run {
        result,
        seen: seen.lock().expect("seen").clone(),
        events: journal
            .records
            .iter()
            .map(|record| record.event.clone())
            .collect(),
        lines: journal.lines.clone(),
        audit: harness.audit().records.clone(),
        deleted: observer.state().deleted.len(),
    };
    assert_eq!(run.deleted, 1, "the sandbox is deleted");
    run
}

async fn scripted(
    responses: Vec<ModelResponse>,
    compactions: Vec<Result<CompactionResponse, ProviderError>>,
    config: HarnessConfig,
) -> Run {
    run_with(responses, compactions, config, false).await
}

fn prompt() -> Message {
    Message::User {
        text: task().prompt,
    }
}

fn resume() -> Message {
    Message::User {
        text: format!("{RESUME_NOTE}{}", task().prompt),
    }
}

/// Turn 1 a tool call over the threshold, a summary before turn 2, then
/// the answer.
async fn compacted_once() -> Run {
    scripted(
        vec![diff("d1", usage(25_000, 10)), answer(usage(500, 20))],
        vec![summarized(usage(25_100, 300))],
        compacting(THRESHOLD),
    )
    .await
}

/// Re-chain `events` and read them back as a journal file.
fn reread(events: Vec<JournalEvent>) -> Result<LoadedJournal, JournalError> {
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("forged.jsonl");
    write_journal_lines(&path, &chained_lines("hs-forged", events));
    read_journal(&path)
}

fn refused_because(events: Vec<JournalEvent>, reason: &str) {
    match reread(events) {
        Err(JournalError::Grammar { reason: found, .. }) => {
            assert!(found.contains(reason), "expected {reason:?}, got {found:?}");
        }
        other => panic!("expected a grammar error {reason:?}, got {other:?}"),
    }
}

fn position(events: &[JournalEvent], wanted: &str) -> usize {
    events
        .iter()
        .position(|event| name(event) == wanted)
        .unwrap_or_else(|| panic!("no {wanted}"))
}

// Engine.

#[tokio::test]
async fn compaction_runs_before_the_turn_it_is_due_and_swaps_the_history() {
    let run = compacted_once().await;
    let outcome = run.result.as_ref().expect("the session completes");
    assert_eq!(outcome.final_answer, "done");
    assert_eq!(
        run.names(),
        [
            "session_started",
            "sandbox_ready",
            "model_requested",
            "model_responded",
            "tool_executed",
            "compaction_requested",
            "compaction_responded",
            "model_requested",
            "model_responded",
            "session_ended",
        ]
    );
    // The summarizer saw the whole history, tool results included; the
    // next turn sees only the summary and the task restated.
    let (Seen::Compact(2, summarized_history), Seen::Complete(2, after)) =
        (&run.seen[1], &run.seen[2])
    else {
        panic!("{:?}", run.seen);
    };
    assert_eq!(summarized_history.len(), 3);
    assert_eq!(summarized_history[0], prompt());
    assert!(matches!(summarized_history[2], Message::ToolResults { .. }));
    assert_eq!(
        after,
        &[
            Message::Compacted {
                summary: SUMMARY.to_string(),
                provider_content: summary_content(SUMMARY),
            },
            resume(),
        ]
    );
    assert!(matches!(
        run.events[5],
        JournalEvent::CompactionRequested {
            turn: 2,
            message_count: 3,
            ..
        }
    ));
    assert!(matches!(
        run.events[7],
        JournalEvent::ModelRequested {
            turn: 2,
            message_count: 2,
            ..
        }
    ));
    // Steps count model turns only; usage includes the compaction's.
    let (work, steps, total, compactions) = run.ended();
    assert_eq!((work, steps, compactions), ("completed", 2, 1));
    assert_eq!(
        total,
        usage(25_000, 10) + usage(25_100, 300) + usage(500, 20)
    );
    assert_eq!(outcome.usage, total);
    assert_eq!(run.compacted_audit(), [(2, "compacted".to_string(), 3)]);
    assert!(run.audit.iter().any(|record| matches!(
        record.event,
        AuditEvent::SessionFinished { compactions: 1, .. }
    )));
    // No summary text reaches the audit.
    let audit = serde_json::to_string(&run.audit).expect("audit");
    assert!(!audit.contains(SUMMARY) && !audit.contains(SIGNATURE));
    run.verified();
}

#[tokio::test]
async fn below_the_threshold_nothing_compacts() {
    // No compaction is scripted: one would end the session.
    let run = scripted(
        vec![diff("d1", usage(10_000, 10)), answer(usage(500, 20))],
        Vec::new(),
        compacting(THRESHOLD),
    )
    .await;
    run.result.as_ref().expect("completes");
    assert!(!run.names().contains(&"compaction_requested"));
    assert_eq!(run.ended().3, 0);
    let journal = run.verified();
    // The policy is recorded even when it never fired.
    assert!(matches!(
        &journal.records[0].event,
        JournalEvent::SessionStarted { compaction: Some(policy), .. }
            if policy.compact_at == THRESHOLD
    ));
}

#[tokio::test]
async fn appended_tool_results_and_truncation_notes_count_toward_the_trigger() {
    // Exactly at the threshold from usage alone: only the bytes pushed
    // after the response make it due.
    let at = usage(19_990, 10);
    let run = scripted(
        vec![diff("d1", at), answer(usage(500, 20))],
        vec![summarized(usage(20_100, 300))],
        compacting(THRESHOLD),
    )
    .await;
    run.result.as_ref().expect("completes");
    assert_eq!(run.ended().3, 1, "the tool results tipped it over");
    run.verified();

    // The same with a discarded truncated turn: the note is what is
    // appended.
    let truncated = ModelResponse {
        stop_reason: StopReason::MaxTokens,
        usage: usage(15_904, 4_096),
        ..ModelResponse::final_text("cut off")
    };
    let run = scripted(
        vec![truncated, answer(usage(500, 20))],
        vec![summarized(usage(20_100, 300))],
        compacting(THRESHOLD),
    )
    .await;
    run.result.as_ref().expect("completes");
    assert_eq!(run.ended().3, 1, "the truncation note tipped it over");
    let Seen::Compact(2, history) = &run.seen[1] else {
        panic!("{:?}", run.seen);
    };
    assert_eq!(history.len(), 2, "the prompt and the note");
    run.verified();

    // Without a policy, nothing compacts however large the prompt.
    let run = scripted(
        vec![diff("d1", usage(900_000, 10)), answer(usage(500, 20))],
        Vec::new(),
        short_lifecycle_config(),
    )
    .await;
    run.result.as_ref().expect("completes");
    assert!(!run.names().contains(&"compaction_requested"));
    // With the flag off, the journal and audit carry no new keys.
    assert!(run.lines.iter().all(|line| !line.contains("compaction")));
    let audit = serde_json::to_string(&run.audit).expect("audit");
    assert!(!audit.contains("compaction"), "{audit}");
}

#[tokio::test]
async fn at_most_sixteen_summaries_per_session() {
    let mut responses = (1..=18)
        .map(|turn| diff(&format!("d{turn}"), usage(25_000, 10)))
        .collect::<Vec<_>>();
    responses.push(answer(usage(500, 20)));
    let compactions = (0..16).map(|_| summarized(usage(2_000, 300))).collect();
    let mut config = compacting(THRESHOLD);
    config.max_total_input_tokens = 10_000_000;
    let run = scripted(responses, compactions, config).await;
    run.result.as_ref().expect("completes");
    let requested = run
        .events
        .iter()
        .filter_map(|event| match event {
            JournalEvent::CompactionRequested { turn, .. } => Some(*turn),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        requested,
        (2..=17).collect::<Vec<_>>(),
        "turns 18 and 19 run on"
    );
    assert_eq!(run.ended().3, 16);
    run.verified();
}

#[tokio::test]
async fn two_attempts_without_a_summary_end_compaction_and_none_retries_in_its_turn() {
    let run = scripted(
        vec![
            diff("d1", usage(25_000, 10)),
            diff("d2", usage(25_000, 10)),
            diff("d3", usage(25_000, 10)),
            answer(usage(500, 20)),
        ],
        vec![no_summary("max_tokens"), no_summary("refusal")],
        compacting(THRESHOLD),
    )
    .await;
    run.result.as_ref().expect("completes");
    let names = run.names();
    // Each attempt is followed by its turn's request, never a second one.
    for (index, event) in names.iter().enumerate() {
        if *event == "compaction_responded" {
            assert_eq!(names[index + 1], "model_requested");
        }
    }
    let attempts = run
        .events
        .iter()
        .filter_map(|event| match event {
            JournalEvent::CompactionResponded { turn, response, .. } => {
                Some((*turn, response.stop_reason.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        attempts,
        [(2, "max_tokens".to_string()), (3, "refusal".to_string())]
    );
    // The history is unchanged after each, so turn 2 sees the tool turn.
    let Seen::Complete(2, history) = &run.seen[2] else {
        panic!("{:?}", run.seen);
    };
    assert_eq!(history.len(), 3);
    assert_eq!(
        run.compacted_audit()
            .into_iter()
            .map(|(_, outcome, _)| outcome)
            .collect::<Vec<_>>(),
        ["no_summary:max_tokens", "no_summary:refusal"]
    );
    let (_, _, total, compactions) = run.ended();
    assert_eq!(compactions, 0);
    // No-summary attempts are billed and counted.
    assert_eq!(total.input_tokens, 3 * 25_000 + 2 * 30_000 + 500);
    run.verified();
}

#[tokio::test]
async fn a_failed_compaction_ends_the_session() {
    let run = scripted(
        vec![diff("d1", usage(25_000, 10)), answer(usage(500, 20))],
        vec![Err(ProviderError::new(
            ProviderErrorKind::Overloaded,
            "HTTP 529 overloaded_error",
        ))],
        compacting(THRESHOLD),
    )
    .await;
    assert!(
        matches!(&run.result, Err(HarnessError::Provider(error)) if error.kind == ProviderErrorKind::Overloaded),
        "{:?}",
        run.result
    );
    let names = run.names();
    assert_eq!(
        &names[names.len() - 3..],
        ["compaction_requested", "compaction_failed", "session_ended"]
    );
    assert_eq!(run.ended().0, "failed");
    run.verified();

    // A malformed response from the provider is a failure too, never
    // journaled as an answer.
    let run = scripted(
        vec![diff("d1", usage(25_000, 10)), answer(usage(500, 20))],
        vec![Ok(CompactionResponse {
            summary: Some("not the block's".to_string()),
            ..summarized(usage(25_100, 300)).expect("summary")
        })],
        compacting(THRESHOLD),
    )
    .await;
    assert!(
        matches!(&run.result, Err(HarnessError::Provider(error)) if error.kind == ProviderErrorKind::InvalidResponse),
        "{:?}",
        run.result
    );
    assert!(run.names().contains(&"compaction_failed"));
    run.verified();
}

#[tokio::test]
async fn cancel_during_compaction_ends_the_session_and_cleans_up() {
    let run = run_with(
        vec![diff("d1", usage(25_000, 10)), answer(usage(500, 20))],
        Vec::new(),
        compacting(THRESHOLD),
        true,
    )
    .await;
    assert!(
        matches!(run.result, Err(HarnessError::Canceled)),
        "{:?}",
        run.result
    );
    let names = run.names();
    assert_eq!(
        &names[names.len() - 2..],
        ["compaction_requested", "session_ended"]
    );
    assert!(matches!(
        run.events.last(),
        Some(JournalEvent::SessionEnded {
            outcome,
            sandbox_deleted: true,
            steps: 1,
            ..
        }) if outcome == "canceled"
    ));
    assert_eq!(run.deleted, 1);
    run.verified();
}

#[tokio::test]
async fn a_token_budget_crossed_by_compaction_usage_ends_the_session() {
    let mut config = compacting(THRESHOLD);
    config.max_total_input_tokens = 30_000;
    let run = scripted(
        vec![diff("d1", usage(25_000, 10)), answer(usage(500, 20))],
        vec![summarized(usage(10_000, 300))],
        config,
    )
    .await;
    assert!(
        matches!(run.result, Err(HarnessError::TokenBudget)),
        "{:?}",
        run.result
    );
    let names = run.names();
    assert_eq!(
        &names[names.len() - 2..],
        ["compaction_responded", "session_ended"]
    );
    let (work, steps, total, compactions) = run.ended();
    assert_eq!((work, steps, compactions), ("failed", 1, 1));
    assert_eq!(total.input_tokens, 35_000);
    run.verified();
}

// Grammar.

#[tokio::test]
async fn the_grammar_refuses_compactions_the_engine_would_not_write() {
    let recorded = compacted_once().await;
    let events = recorded.events.clone();
    reread(events.clone()).expect("re-chained, the engine's journal verifies");
    let requested = position(&events, "compaction_requested");

    // Due but skipped: the request follows the tool turn directly.
    let mut skipped = events.clone();
    skipped.drain(requested..requested + 2);
    refused_because(skipped, "model_requested when compaction was due");

    // A tool result changed before the swap, the chain re-hashed: the
    // summarized history no longer matches.
    let mut tampered = events.clone();
    let executed = position(&tampered, "tool_executed");
    if let JournalEvent::ToolExecuted { result, .. } = &mut tampered[executed] {
        result["tampered"] = json!(true);
    }
    refused_because(tampered, "compaction_requested hash differs");

    let responded = position(&events, "compaction_responded");
    let mut different = events.clone();
    if let JournalEvent::CompactionResponded { response, .. } = &mut different[responded] {
        response.summary = Some("another summary".to_string());
    }
    refused_because(different, "differs from its block's content");

    let mut two_blocks = events.clone();
    if let JournalEvent::CompactionResponded { response, .. } = &mut two_blocks[responded] {
        let content = response.provider_content.as_mut().expect("content");
        let block = content.blocks[0].clone();
        content.blocks = json!([block.clone(), block]);
    }
    refused_because(two_blocks, "not exactly one content block");

    let mut foreign = events.clone();
    if let JournalEvent::CompactionResponded { response, .. } = &mut foreign[responded] {
        response
            .provider_content
            .as_mut()
            .expect("content")
            .provider = "other".to_string();
    }
    refused_because(foreign, "not from anthropic");

    let mut wrong_count = events.clone();
    if let JournalEvent::ModelRequested { message_count, .. } = &mut wrong_count[responded + 1] {
        *message_count = 4;
    }
    refused_because(wrong_count, "model_requested message_count differs");

    let mut total = events.clone();
    let last = total.len() - 1;
    if let JournalEvent::SessionEnded { compactions, .. } = &mut total[last] {
        *compactions = 0;
    }
    refused_because(total, "compactions differs");

    let mut uncounted = events.clone();
    if let JournalEvent::SessionEnded { usage, .. } = &mut uncounted[last] {
        *usage = usage::default_sum(&events);
    }
    refused_because(uncounted, "usage differs");

    // An unanswered compaction may not end in completed work, even with
    // every count made to agree.
    let mut unanswered = events.clone();
    unanswered.drain(responded..last);
    let last = unanswered.len() - 1;
    if let JournalEvent::SessionEnded {
        steps,
        usage,
        compactions,
        ..
    } = &mut unanswered[last]
    {
        *steps = 1;
        *usage = self::usage(25_000, 10);
        *compactions = 0;
    }
    refused_because(unanswered, "completed work after an unanswered");

    // Without a recorded policy no compaction is allowed at all.
    let mut no_policy = events.clone();
    if let JournalEvent::SessionStarted { compaction, .. } = &mut no_policy[0] {
        *compaction = None;
    }
    refused_because(no_policy, "without a recorded compaction policy");

    let mut bad_policy = events;
    if let JournalEvent::SessionStarted { compaction, .. } = &mut bad_policy[0] {
        compaction.as_mut().expect("policy").compact_at = 5;
    }
    refused_because(bad_policy, "recorded compaction policy is invalid");
}

/// The sum of the model responses' usage only, as a journal that forgot
/// the compaction would claim.
mod usage {
    use super::{JournalEvent, Usage};

    pub fn default_sum(events: &[JournalEvent]) -> Usage {
        events
            .iter()
            .fold(Usage::default(), |sum, event| match event {
                JournalEvent::ModelResponded { response, .. } => sum + response.usage,
                _ => sum,
            })
    }
}

#[tokio::test]
async fn the_grammar_refuses_a_compaction_that_was_not_due() {
    let below = scripted(
        vec![diff("d1", usage(10_000, 10)), answer(usage(500, 20))],
        Vec::new(),
        compacting(THRESHOLD),
    )
    .await;
    let mut events = below.events.clone();
    let second = events
        .iter()
        .rposition(|event| matches!(event, JournalEvent::ModelRequested { .. }))
        .expect("turn 2");
    events.insert(
        second,
        JournalEvent::CompactionRequested {
            turn: 2,
            request_sha256: "0".repeat(64),
            message_count: 3,
        },
    );
    refused_because(events, "compaction_requested when compaction was not due");
}

#[tokio::test]
async fn the_grammar_refuses_a_second_attempt_and_a_request_after_a_budget() {
    let run_twice = scripted(
        vec![
            diff("d1", usage(25_000, 10)),
            diff("d2", usage(25_000, 10)),
            answer(usage(500, 20)),
        ],
        vec![no_summary("end_turn"), no_summary("end_turn")],
        compacting(THRESHOLD),
    )
    .await;
    let mut events = run_twice.events.clone();
    let requested = position(&events, "compaction_requested");
    let again = events[requested].clone();
    events.insert(requested + 2, again);
    refused_because(events, "a second compaction_requested in one turn");

    let mut config = compacting(THRESHOLD);
    config.max_total_input_tokens = 30_000;
    let over = scripted(
        vec![diff("d1", usage(25_000, 10)), answer(usage(500, 20))],
        vec![summarized(usage(10_000, 300))],
        config,
    )
    .await;
    let mut events = over.events.clone();
    let ended = events.len() - 1;
    events.insert(
        ended,
        JournalEvent::ModelRequested {
            turn: 2,
            request_sha256: "0".repeat(64),
            message_count: 2,
        },
    );
    refused_because(
        events,
        "model_requested after a compaction response over a budget",
    );
}

/// The compaction request of `events`'s first compaction, rebuilt from
/// what the provider saw.
fn recompute_compaction_hash(run: &Run, events: &mut [JournalEvent], instructions: &str) {
    let JournalEvent::SessionStarted {
        system,
        tools,
        limits,
        ..
    } = &events[0]
    else {
        panic!("session_started first");
    };
    let Some(Seen::Compact(turn, messages)) = run
        .seen
        .iter()
        .find(|seen| matches!(seen, Seen::Compact(..)))
    else {
        panic!("no compaction seen");
    };
    let hash = compaction_request_sha256(
        &ModelRequest {
            session_id: "",
            turn: *turn,
            system,
            tools,
            messages,
            max_output_tokens: limits.max_output_tokens_per_request,
        },
        instructions,
    );
    let requested = position(events, "compaction_requested");
    if let JournalEvent::CompactionRequested { request_sha256, .. } = &mut events[requested] {
        *request_sha256 = hash;
    }
}

#[tokio::test]
async fn other_instructions_verify_but_do_not_replay_and_are_bound_by_the_hash() {
    let recorded = compacted_once().await;
    let other = format!("{:x}", Sha256::digest(b"another build's instructions"));
    // The engine's hash binds the recorded instructions hash.
    let mut same = recorded.events.clone();
    recompute_compaction_hash(&recorded, &mut same, &instructions_sha256());
    assert_eq!(same, recorded.events);

    // A journal from a build with other instructions verifies.
    let mut older = recorded.events.clone();
    if let JournalEvent::SessionStarted { compaction, .. } = &mut older[0] {
        compaction.as_mut().expect("policy").instructions_sha256 = other.clone();
    }
    let mut rebound = older.clone();
    recompute_compaction_hash(&recorded, &mut rebound, &other);
    let journal = reread(rebound).expect("another build's journal verifies");

    // Editing the recorded instructions hash without re-hashing breaks it.
    refused_because(older, "compaction_requested hash differs");

    // Replay refuses it before any sandbox exists.
    let replay = ReplayProvider::from_journal(&journal).expect("loads");
    let problem = replay.compaction_problem().expect("refused");
    assert!(problem.contains(&other), "{problem}");
    let config = replay.replay_config(short_lifecycle_config());
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        replay,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let error = harness
        .replay(CancellationToken::new())
        .await
        .expect_err("refused");
    assert!(error.to_string().contains(&other), "{error}");
    assert!(api.state().creates.is_empty(), "no sandbox was made");

    // Without replay_config too: the replay itself refuses it, even for a
    // harness built with the caller's own configuration.
    let replay = ReplayProvider::from_journal(&journal).expect("loads");
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        replay,
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );
    let error = harness
        .replay(CancellationToken::new())
        .await
        .expect_err("refused");
    assert!(error.to_string().contains(&other), "{error}");
    assert!(api.state().creates.is_empty(), "no sandbox was made");
}

#[tokio::test]
async fn replay_refuses_a_policy_other_than_the_recorded_one() {
    let recorded = compacted_once().await;
    let journal = recorded.verified();
    for config in [short_lifecycle_config(), compacting(THRESHOLD + 1)] {
        let replay = ReplayProvider::from_journal(&journal).expect("replayable");
        let api = FakeApi::default();
        let mut harness = Harness::new(
            api.clone(),
            replay,
            MemoryAudit::default(),
            MemoryJournal::default(),
            config,
        );
        let error = harness
            .replay(CancellationToken::new())
            .await
            .expect_err("refused");
        assert!(error.to_string().contains("replay_config"), "{error}");
        assert!(api.state().creates.is_empty(), "no sandbox was made");
    }
}

// Replay.

#[tokio::test]
async fn a_recorded_compaction_replays_without_a_model() {
    let recorded = compacted_once().await;
    let journal = recorded.verified();
    let replay = ReplayProvider::from_journal(&journal).expect("replayable");
    assert_eq!(replay.compaction(), Some(&CompactionPolicy::new(THRESHOLD)));
    let config = replay.replay_config(short_lifecycle_config());
    assert_eq!(config.compaction, Some(CompactionPolicy::new(THRESHOLD)));
    let mut harness = Harness::new(
        FakeApi::default(),
        replay,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let outcome = harness
        .replay(CancellationToken::new())
        .await
        .expect("replays");
    assert_eq!(outcome.final_answer, "done");
    assert_eq!(harness.provider().diverged_at(), None);
    assert!(harness.provider().exhausted(), "every attempt consumed");
    // The replay journals the same records, so it verifies the same way.
    let names = harness
        .journal()
        .records
        .iter()
        .map(|record| name(&record.event))
        .collect::<Vec<_>>();
    assert_eq!(names, recorded.names());
}

fn turn_one(journal: &LoadedJournal) -> (String, Vec<ToolDefinition>, u32) {
    let JournalEvent::SessionStarted {
        system,
        tools,
        limits,
        ..
    } = &journal.records[0].event
    else {
        panic!("session_started first");
    };
    (
        system.clone(),
        tools.clone(),
        limits.max_output_tokens_per_request,
    )
}

#[tokio::test]
async fn replay_diverges_on_order_and_on_the_compaction_hash() {
    let recorded = compacted_once().await;
    let journal = recorded.verified();
    let (system, tools, max_output_tokens) = turn_one(&journal);
    let first = [prompt()];
    let turn_one_request = ModelRequest {
        session_id: "replay",
        turn: 1,
        system: &system,
        tools: &tools,
        messages: &first,
        max_output_tokens,
    };
    let Some(Seen::Compact(_, compacted)) = recorded.seen.get(1).cloned() else {
        panic!("{:?}", recorded.seen);
    };

    // A model request where the recording compacted.
    let mut replay = ReplayProvider::from_journal(&journal).expect("loads");
    replay.complete(turn_one_request).await.expect("turn 1");
    let error = replay
        .complete(ModelRequest {
            turn: 2,
            messages: &compacted,
            ..turn_one_request
        })
        .await
        .expect_err("diverged");
    assert_eq!(error.kind, ProviderErrorKind::ReplayDiverged);
    assert!(error.message.contains("recording compacted"), "{error}");
    assert_eq!(replay.diverged_at(), Some(2));

    // A compaction where the recording made a model request.
    let mut replay = ReplayProvider::from_journal(&journal).expect("loads");
    let error = replay
        .compact(turn_one_request)
        .await
        .expect_err("diverged");
    assert_eq!(error.kind, ProviderErrorKind::ReplayDiverged);
    assert!(error.message.contains("requested a model turn"), "{error}");

    // The same turn's compaction over a different history.
    let mut replay = ReplayProvider::from_journal(&journal).expect("loads");
    replay.complete(turn_one_request).await.expect("turn 1");
    let error = replay
        .compact(ModelRequest {
            turn: 2,
            messages: &compacted[..2],
            ..turn_one_request
        })
        .await
        .expect_err("diverged");
    assert_eq!(error.kind, ProviderErrorKind::ReplayDiverged);
    assert!(
        error.message.contains("compaction request differs"),
        "{error}"
    );
    assert!(!replay.exhausted());

    // And the matching one is answered from the recording.
    let mut replay = ReplayProvider::from_journal(&journal).expect("loads");
    replay.complete(turn_one_request).await.expect("turn 1");
    let response = replay
        .compact(ModelRequest {
            turn: 2,
            messages: &compacted,
            ..turn_one_request
        })
        .await
        .expect("recorded summary");
    assert_eq!(response.summary.as_deref(), Some(SUMMARY));

    // Through the engine, a replay without the recorded policy is refused
    // before it starts (the provider-level calls above show the divergence
    // it would otherwise reach at the compaction it no longer asks for).
    let replay = ReplayProvider::from_journal(&journal).expect("loads");
    let mut config = replay.replay_config(short_lifecycle_config());
    config.compaction = None;
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        replay,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let error = harness
        .replay(CancellationToken::new())
        .await
        .expect_err("refused");
    assert!(matches!(error, HarnessError::InvalidTask(_)), "{error}");
    assert!(api.state().creates.is_empty(), "no sandbox was made");
}

#[tokio::test]
async fn an_unanswered_recorded_compaction_keeps_the_replay_unexhausted() {
    let canceled = run_with(
        vec![diff("d1", usage(25_000, 10)), answer(usage(500, 20))],
        Vec::new(),
        compacting(THRESHOLD),
        true,
    )
    .await;
    let journal = canceled.verified();
    let mut replay = ReplayProvider::from_journal(&journal).expect("loads");
    let (system, tools, max_output_tokens) = turn_one(&journal);
    let first = [prompt()];
    let turn_one_request = ModelRequest {
        session_id: "replay",
        turn: 1,
        system: &system,
        tools: &tools,
        messages: &first,
        max_output_tokens,
    };
    replay.complete(turn_one_request).await.expect("turn 1");
    let Some(JournalEvent::ToolExecuted {
        turn,
        index,
        call,
        ok,
        result,
        ..
    }) = journal
        .records
        .iter()
        .map(|record| &record.event)
        .find(|event| matches!(event, JournalEvent::ToolExecuted { .. }))
    else {
        panic!("a tool ran");
    };
    replay
        .tool_completed(*turn, *index, call, *ok, result)
        .expect("same tool");
    assert!(!replay.exhausted(), "the unanswered compaction is left");
    let Some(Seen::Compact(_, history)) = canceled.seen.get(1).cloned() else {
        panic!("{:?}", canceled.seen);
    };
    let error = replay
        .compact(ModelRequest {
            turn: 2,
            messages: &history,
            ..turn_one_request
        })
        .await
        .expect_err("no answer recorded");
    assert_eq!(error.kind, ProviderErrorKind::Script);
    assert!(replay.exhausted());
}

// The Anthropic adapter.

const SENTINEL_KEY: &str = "sk-ant-SENTINEL-compaction-77";

fn anthropic(server: &FakeHttp) -> AnthropicConfig {
    let mut config = AnthropicConfig::new(
        &format!("http://{}", server.addr),
        "claude-opus-5-5",
        Some(SecretString::new(SENTINEL_KEY)),
    )
    .expect("config");
    config.retry_base_delay = Duration::from_millis(5);
    config.retry_max_delay = Duration::from_millis(20);
    config
}

fn compaction_message(content: &Value, stop: &str, used: &Value) -> Value {
    json!({
        "id": "msg_01C",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": content,
        "stop_reason": stop,
        "stop_details": null,
        "usage": used,
    })
}

fn iterations(entries: &[(u64, u64, u64)]) -> Value {
    json!({
        "input_tokens": 0,
        "output_tokens": 0,
        "iterations": entries
            .iter()
            .map(|(input, output, cache_read)| json!({
                "type": "compaction",
                "input_tokens": input,
                "output_tokens": output,
                "cache_read_input_tokens": cache_read,
            }))
            .collect::<Vec<_>>(),
    })
}

fn block(summary: &str, signature: &str) -> Value {
    json!({"type": "compaction", "content": summary, "signature": signature})
}

fn summary_reply(signature: &str) -> Reply {
    Reply::json(
        200,
        &compaction_message(
            &json!([block(SUMMARY, signature)]),
            "compaction",
            &iterations(&[(25_100, 300, 0)]),
        ),
    )
}

fn assistant_turn() -> Message {
    Message::Assistant {
        text: None,
        tool_calls: vec![ToolCall::new("toolu_01D", "workspace_diff", json!({}))],
        provider_content: Some(ProviderContent {
            provider: "anthropic".to_string(),
            model: Some("claude-opus-5-5".to_string()),
            blocks: json!([{"type": "tool_use", "id": "toolu_01D", "name": "workspace_diff",
                            "input": {}}]),
            stop_details: None,
        }),
    }
}

fn history() -> Vec<Message> {
    vec![
        prompt(),
        assistant_turn(),
        Message::ToolResults {
            results: vec![kiln_harness::provider::ToolResult {
                call_id: "toolu_01D".to_string(),
                tool: "workspace_diff".to_string(),
                result: json!({"ok": true, "added": []}),
            }],
        },
    ]
}

fn tools() -> Vec<ToolDefinition> {
    kiln_harness::tools::ToolRegistry::new(kiln_harness::tools::ToolLimits::default())
        .definitions()
        .to_vec()
}

fn request<'a>(messages: &'a [Message], tools: &'a [ToolDefinition]) -> ModelRequest<'a> {
    ModelRequest {
        session_id: "hs-compaction",
        turn: 2,
        system: "You are careful.",
        tools,
        messages,
        max_output_tokens: 4096,
    }
}

#[tokio::test]
async fn the_compaction_request_is_the_turn_request_plus_compaction_and_its_beta_only() {
    let server = FakeHttp::start([summary_reply(SIGNATURE)]).await;
    // Fallbacks and clearing are refused with --compact-at; even when set
    // here, the compaction request carries neither.
    let mut config = anthropic(&server);
    config.fallbacks = true;
    config.clear_thinking = Some(2);
    let messages = history();
    let tools = tools();
    let mut provider = AnthropicProvider::new(config.clone()).expect("provider");
    let response = provider
        .compact(request(&messages, &tools))
        .await
        .expect("summary");
    assert_eq!(response.summary.as_deref(), Some(SUMMARY));
    let sent = &server.requests()[0];
    assert_eq!(sent.header("anthropic-beta"), Some(COMPACTION_BETA));
    let body = sent.json();
    let mut expected = request_body(&config, &request(&messages, &tools)).expect("body");
    let map = expected.as_object_mut().expect("object");
    map.remove("context_management");
    map.remove("fallbacks");
    map.insert(
        "compaction".to_string(),
        json!({"type": "summarize", "instructions": COMPACTION_INSTRUCTIONS}),
    );
    assert_eq!(body, expected);
    assert_eq!(
        body,
        compaction_body(&config, &request(&messages, &tools)).expect("body")
    );
    for kept in [
        "cache_control",
        "system",
        "tools",
        "output_config",
        "max_tokens",
    ] {
        assert!(body.get(kept).is_some(), "{kept}");
    }
    // The instructions on the wire hash to the policy's recorded value,
    // which every compaction_requested binds.
    let sent_instructions = body["compaction"]["instructions"].as_str().expect("text");
    assert_eq!(
        format!("{:x}", Sha256::digest(sent_instructions.as_bytes())),
        CompactionPolicy::new(THRESHOLD).instructions_sha256
    );
    assert!(!String::from_utf8_lossy(&sent.body).contains(SENTINEL_KEY));
}

#[tokio::test]
async fn requests_after_a_swap_carry_the_block_verbatim_and_the_beta() {
    let content = json!([text_reply("done")]);
    for (fallbacks, betas) in [
        (false, COMPACTION_BETA.to_string()),
        (true, format!("{FALLBACK_BETA},{COMPACTION_BETA}")),
    ] {
        let server = FakeHttp::start([Reply::json(
            200,
            &compaction_message(
                &content,
                "end_turn",
                &common::anthropic::live_usage(400, 20, 0, 0),
            ),
        )])
        .await;
        let mut config = anthropic(&server);
        config.fallbacks = fallbacks;
        let mut provider = AnthropicProvider::new(config).expect("provider");
        let swapped = [
            Message::Compacted {
                summary: SUMMARY.to_string(),
                provider_content: summary_content(SUMMARY),
            },
            resume(),
        ];
        let tools = tools();
        provider
            .complete(request(&swapped, &tools))
            .await
            .expect("answer");
        let sent = &server.requests()[0];
        assert_eq!(sent.header("anthropic-beta"), Some(betas.as_str()));
        let body = sent.json();
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(
            serde_json::to_string(&messages[0]).expect("encode"),
            serde_json::to_string(&json!({
                "role": "assistant",
                "content": summary_content(SUMMARY).blocks,
            }))
            .expect("encode")
        );
        assert_eq!(
            messages[1],
            json!({"role": "user", "content": format!("{RESUME_NOTE}{}", task().prompt)})
        );
        assert!(body.get("compaction").is_none());
    }

    // Without a summary in the history the headers are as before.
    let server = FakeHttp::start([Reply::json(
        200,
        &compaction_message(
            &content,
            "end_turn",
            &common::anthropic::live_usage(400, 20, 0, 0),
        ),
    )])
    .await;
    let mut provider = AnthropicProvider::new(anthropic(&server)).expect("provider");
    let messages = [prompt()];
    let tools = tools();
    provider
        .complete(request(&messages, &tools))
        .await
        .expect("answer");
    assert_eq!(server.requests()[0].header("anthropic-beta"), None);

    // Another provider's summary is never sent.
    let foreign = [Message::Compacted {
        summary: SUMMARY.to_string(),
        provider_content: ProviderContent {
            provider: "other".to_string(),
            ..summary_content(SUMMARY)
        },
    }];
    let config = AnthropicConfig::new(
        "https://api.anthropic.com",
        "claude-opus-5-5",
        Some(SecretString::new(SENTINEL_KEY)),
    )
    .expect("config");
    let error = request_body(&config, &request(&foreign, &tools)).expect_err("refused");
    assert_eq!(error.kind, ProviderErrorKind::Rejected);
}

fn text_reply(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn parsed(content: &Value, stop: &str, used: &Value) -> Result<CompactionResponse, ProviderError> {
    parse_compaction(&compaction_message(content, stop, used), "claude-opus-5-5")
}

#[test]
fn a_summary_parses_with_usage_summed_from_iterations() {
    let response = parsed(
        &json!([block(SUMMARY, SIGNATURE)]),
        "compaction",
        &iterations(&[(10_000, 200, 5_000), (3_000, 100, 0)]),
    )
    .expect("summary");
    assert!(response.is_success());
    assert_eq!(response.summary.as_deref(), Some(SUMMARY));
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 13_000,
            output_tokens: 300,
            cache_read_input_tokens: 5_000,
            cache_creation_input_tokens: 0,
        }
    );
    let content = response.provider_content.expect("content");
    assert_eq!(content.blocks, json!([block(SUMMARY, SIGNATURE)]));
    assert_eq!(content.model.as_deref(), Some("claude-opus-5-5"));

    // Fully read from cache.
    let cached = parsed(
        &json!([block(SUMMARY, SIGNATURE)]),
        "compaction",
        &iterations(&[(0, 200, 25_000)]),
    )
    .expect("cached summary");
    assert_eq!(cached.usage.total_input_tokens(), 25_000);
}

#[test]
fn every_stop_without_a_summary_needs_empty_content_and_may_cost_nothing() {
    for stop in [
        "max_tokens",
        "model_context_window_exceeded",
        "tool_use",
        "refusal",
        "end_turn",
    ] {
        let response = parsed(&json!([]), stop, &iterations(&[(20_000, 80, 0)])).expect(stop);
        assert_eq!(response.stop_reason, stop);
        assert!(!response.is_success());
        assert_eq!((response.summary, response.provider_content), (None, None));
        assert_eq!(response.usage, usage(20_000, 80));

        let error = parsed(
            &json!([text_reply("partial")]),
            stop,
            &iterations(&[(20_000, 80, 0)]),
        )
        .expect_err("content without a summary");
        assert_eq!(error.kind, ProviderErrorKind::InvalidResponse, "{stop}");
    }
    let free = parsed(&json!([]), "refusal", &iterations(&[(0, 0, 0)])).expect("zero usage");
    assert_eq!(free.usage, Usage::default());
}

#[test]
fn malformed_compaction_responses_are_invalid() {
    let good_usage = iterations(&[(25_000, 300, 0)]);
    let cases = [
        (json!([]), "compaction", good_usage.clone()),
        (
            json!([block(SUMMARY, SIGNATURE), block(SUMMARY, SIGNATURE)]),
            "compaction",
            good_usage.clone(),
        ),
        (
            json!([text_reply(SUMMARY)]),
            "compaction",
            good_usage.clone(),
        ),
        (
            json!([{"type": "compaction", "content": SUMMARY}]),
            "compaction",
            good_usage.clone(),
        ),
        (
            json!([{"type": "compaction", "content": 7, "signature": SIGNATURE}]),
            "compaction",
            good_usage.clone(),
        ),
        (
            json!([block("", SIGNATURE)]),
            "compaction",
            good_usage.clone(),
        ),
        (
            json!([{"type": "compaction", "content": SUMMARY, "signature": SIGNATURE,
                    "weight": 0.5}]),
            "compaction",
            good_usage.clone(),
        ),
        (json!([]), "End Turn", good_usage.clone()),
        // Usage: iterations required, compaction entries only, and a
        // plausible floor for the summary.
        (
            json!([block(SUMMARY, SIGNATURE)]),
            "compaction",
            json!({"input_tokens": 25_000, "output_tokens": 300}),
        ),
        (
            json!([block(SUMMARY, SIGNATURE)]),
            "compaction",
            json!({"input_tokens": 0, "output_tokens": 0, "iterations": [
                {"type": "message", "input_tokens": 25_000, "output_tokens": 300}]}),
        ),
        (
            json!([block(SUMMARY, SIGNATURE)]),
            "compaction",
            json!({"input_tokens": 0, "output_tokens": 0, "iterations": [
                {"type": "compaction", "model": "claude-other", "input_tokens": 25_000,
                 "output_tokens": 300}]}),
        ),
        (
            json!([block(SUMMARY, SIGNATURE)]),
            "compaction",
            iterations(&[(25_000, 0, 0)]),
        ),
        (
            json!([block(SUMMARY, SIGNATURE)]),
            "compaction",
            iterations(&[(0, 300, 0)]),
        ),
    ];
    for (content, stop, used) in cases {
        let error = parsed(&content, stop, &used).expect_err("invalid");
        assert_eq!(
            error.kind,
            ProviderErrorKind::InvalidResponse,
            "{content} {stop} {used}"
        );
    }
    // A summary from another model is an unmarked switch.
    let mut other = compaction_message(
        &json!([block(SUMMARY, SIGNATURE)]),
        "compaction",
        &good_usage,
    );
    other["model"] = json!("claude-sonnet-5-5");
    let error = parse_compaction(&other, "claude-opus-5-5").expect_err("switch");
    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
}

#[tokio::test]
async fn a_credential_anywhere_in_a_summary_refuses_it_whole() {
    for signature in [
        format!("Eu{SENTINEL_KEY}=="),
        // JSON-escaped spellings decode to the same key.
        format!("Eu{}==", SENTINEL_KEY.replace('S', "\\u0053")),
    ] {
        let body = compaction_message(
            &json!([block(SUMMARY, "PLACEHOLDER")]),
            "compaction",
            &iterations(&[(25_100, 300, 0)]),
        )
        .to_string()
        .replace("PLACEHOLDER", &signature);
        let server = FakeHttp::start([Reply::status(200, &body)]).await;
        let mut provider = AnthropicProvider::new(anthropic(&server)).expect("provider");
        let messages = history();
        let tools = tools();
        let error = provider
            .compact(request(&messages, &tools))
            .await
            .expect_err("refused");
        assert_eq!(error.kind, ProviderErrorKind::InvalidResponse, "{error}");
        assert!(!error.message.contains(SENTINEL_KEY));
        assert_eq!(server.requests().len(), 1, "never retried");
    }
}

#[tokio::test]
async fn compaction_errors_are_classified_and_retried_like_turns() {
    let overloaded = || {
        Reply::status(
            529,
            &json!({"type": "error", "error": {"type": "overloaded_error",
                    "message": "compaction_unavailable"}})
            .to_string(),
        )
    };
    let server = FakeHttp::start([overloaded(), summary_reply(SIGNATURE)]).await;
    let mut provider = AnthropicProvider::new(anthropic(&server)).expect("provider");
    let messages = history();
    let tools = tools();
    provider
        .compact(request(&messages, &tools))
        .await
        .expect("retried");
    assert_eq!(server.requests().len(), 2);

    let server = FakeHttp::start([overloaded(), overloaded(), overloaded()]).await;
    let mut provider = AnthropicProvider::new(anthropic(&server)).expect("provider");
    let error = provider
        .compact(request(&messages, &tools))
        .await
        .expect_err("attempts run out");
    assert_eq!(error.kind, ProviderErrorKind::Overloaded);

    // A provider without compaction refuses.
    let mut scripted = ScriptedProvider::new([]);
    let mut plain = PlainProvider(&mut scripted);
    let error = plain
        .compact(request(&messages, &tools))
        .await
        .expect_err("unsupported");
    assert_eq!(error.kind, ProviderErrorKind::Rejected);
    assert_eq!(error.message, "compaction is unsupported");
}

/// A provider that keeps the trait's default `compact`.
struct PlainProvider<'a>(&'a mut ScriptedProvider);

#[async_trait]
impl Provider for PlainProvider<'_> {
    fn info(&self) -> ProviderInfo {
        self.0.info()
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.0.complete(request).await
    }
}

/// A whole session through the adapter: the summary is requested, echoed
/// first on the next turn with the beta, journaled, verified and replayed
/// with the server gone.
#[tokio::test]
async fn a_live_shaped_session_compacts_verifies_and_replays_without_the_server() {
    let turn_one = Reply::json(
        200,
        &compaction_message(
            &json!([{"type": "tool_use", "id": "toolu_01D", "name": "workspace_diff",
                     "input": {}}]),
            "tool_use",
            &common::anthropic::live_usage(25_000, 40, 0, 0),
        ),
    );
    let turn_two = Reply::json(
        200,
        &compaction_message(
            &json!([text_reply("done")]),
            "end_turn",
            &common::anthropic::live_usage(600, 20, 0, 0),
        ),
    );
    let server = FakeHttp::start([turn_one, summary_reply(SIGNATURE), turn_two]).await;
    let mut harness = Harness::new(
        FakeApi::default(),
        AnthropicProvider::new(anthropic(&server)).expect("provider"),
        MemoryAudit::default(),
        MemoryJournal::default(),
        compacting(THRESHOLD),
    );
    let outcome = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("completes");
    assert_eq!(outcome.final_answer, "done");
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].header("anthropic-beta"), None);
    assert_eq!(requests[1].header("anthropic-beta"), Some(COMPACTION_BETA));
    assert_eq!(requests[2].header("anthropic-beta"), Some(COMPACTION_BETA));
    assert!(requests[1].json().get("compaction").is_some());
    let last = requests[2].json();
    assert_eq!(
        last["messages"][0]["content"],
        json!([block(SUMMARY, SIGNATURE)])
    );
    assert_eq!(last["messages"].as_array().map(Vec::len), Some(2));
    // The summarized prefix is the previous request's, plus the results.
    let first = requests[0].json();
    let compaction = requests[1].json();
    assert_eq!(compaction["system"], first["system"]);
    assert_eq!(compaction["tools"], first["tools"]);
    assert_eq!(compaction["messages"][0], first["messages"][0]);

    let lines = harness.journal().lines.clone();
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("run.jsonl");
    write_journal_lines(&path, &lines);
    let journal = read_journal(&path).expect("verifies");
    server.stop();
    let replay = ReplayProvider::from_journal(&journal).expect("replayable");
    let config = replay.replay_config(short_lifecycle_config());
    let mut harness = Harness::new(
        FakeApi::default(),
        replay,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let replayed = harness
        .replay(CancellationToken::new())
        .await
        .expect("replays");
    assert_eq!(replayed.final_answer, "done");
    assert!(harness.provider().exhausted());
    assert_eq!(harness.provider().diverged_at(), None);
}

#[tokio::test]
async fn journal_verify_counts_compactions_and_replay_refuses_other_instructions_offline() {
    let recorded = compacted_once().await;
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("run.jsonl");
    write_journal_lines(&path, &recorded.lines);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
        .args(["journal", "verify"])
        .arg(&path)
        .output()
        .expect("verify");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("\nturns: 2\n"), "{stdout}");
    assert!(stdout.contains("\ncompactions: 1\n"), "{stdout}");
    // 25,000 + 25,100 + 500 input and 10 + 300 + 20 output.
    assert!(
        stdout.contains("usage: input 50600, output 330, cache read 0, cache creation 0"),
        "{stdout}"
    );

    // Another build's instructions: it verifies, and replay refuses it
    // before kilnd is contacted.
    let other = format!("{:x}", Sha256::digest(b"another build's instructions"));
    let mut events = recorded.events.clone();
    if let JournalEvent::SessionStarted { compaction, .. } = &mut events[0] {
        compaction.as_mut().expect("policy").instructions_sha256 = other.clone();
    }
    recompute_compaction_hash(&recorded, &mut events, &other);
    let older = temp.path().join("older.jsonl");
    write_journal_lines(&older, &chained_lines("hs-older", events));
    let verified = std::process::Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
        .args(["journal", "verify"])
        .arg(&older)
        .output()
        .expect("verify");
    assert!(verified.status.success(), "{verified:?}");
    let replayed = std::process::Command::new(env!("CARGO_BIN_EXE_kiln-harness"))
        .arg("--socket")
        .arg(temp.path().join("no-kilnd.sock"))
        .args(["replay", "--from"])
        .arg(&older)
        .arg("--audit")
        .arg(temp.path().join("out/audit.jsonl"))
        .arg("--journal")
        .arg(temp.path().join("out/replay.jsonl"))
        .output()
        .expect("replay");
    let stderr = String::from_utf8_lossy(&replayed.stderr);
    assert!(!replayed.status.success(), "{stderr}");
    assert!(stderr.contains("cannot replay"), "{stderr}");
    assert!(stderr.contains(&other), "{stderr}");
    assert!(!temp.path().join("out").exists(), "nothing was opened");
}
