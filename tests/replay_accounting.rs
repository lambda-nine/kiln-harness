//! Replay accounts for every recorded request, answered or not, and locates
//! every request the recording does not have.

use kiln_harness::engine::HarnessConfig;
use kiln_harness::journal::{
    JOURNAL_SCHEMA, JournalEvent, JournalRecord, LoadedJournal, request_sha256,
};
use kiln_harness::provider::{
    Message, ModelRequest, ModelResponse, Provider, ProviderErrorKind, ProviderInfo, Usage,
};
use kiln_harness::replay::ReplayProvider;
use kiln_harness::tools::{ToolLimits, ToolRegistry};

struct Fixture {
    system: String,
    tools: Vec<kiln_harness::provider::ToolDefinition>,
    messages: Vec<Message>,
    max_output_tokens: u32,
}

impl Fixture {
    fn new() -> Self {
        Self {
            system: "system".to_string(),
            tools: ToolRegistry::new(ToolLimits::default())
                .definitions()
                .to_vec(),
            messages: vec![Message::User {
                text: "task".to_string(),
            }],
            max_output_tokens: HarnessConfig::default().max_output_tokens_per_request,
        }
    }

    fn request(&self, turn: u32) -> ModelRequest<'_> {
        ModelRequest {
            session_id: "fresh",
            turn,
            system: &self.system,
            tools: &self.tools,
            messages: &self.messages,
            max_output_tokens: self.max_output_tokens,
        }
    }

    /// A one-turn journal: start, sandbox, request 1, then `tail`.
    fn journal(&self, tail: Vec<JournalEvent>) -> LoadedJournal {
        let mut events = vec![
            JournalEvent::SessionStarted {
                schema: JOURNAL_SCHEMA,
                harness_version: "0.1.0".to_string(),
                provider: ProviderInfo {
                    name: "test".to_string(),
                    model: "test".to_string(),
                },
                system: self.system.clone(),
                task: "task".to_string(),
                tools: self.tools.clone(),
                limits: HarnessConfig::default().limits(),
            },
            JournalEvent::SandboxReady {
                sandbox_id: "sb-1".to_string(),
                baseline_file_count: 0,
            },
            JournalEvent::ModelRequested {
                turn: 1,
                request_sha256: request_sha256(&self.request(1)),
                message_count: 1,
            },
        ];
        events.extend(tail);
        LoadedJournal {
            records: events
                .into_iter()
                .enumerate()
                .map(|(sequence, event)| JournalRecord {
                    sequence: sequence as u64,
                    unix_ms: 0,
                    session_id: "recorded".to_string(),
                    prev_sha256: String::new(),
                    event,
                })
                .collect(),
            complete: true,
        }
    }
}

fn ended(outcome: &str, final_answer: Option<&str>) -> JournalEvent {
    JournalEvent::SessionEnded {
        outcome: outcome.to_string(),
        work_outcome: outcome.to_string(),
        final_answer: final_answer.map(str::to_string),
        steps: 1,
        tool_calls: 0,
        usage: Usage::default(),
        sandbox_deleted: true,
    }
}

#[tokio::test]
async fn a_request_past_the_recording_is_located() {
    let fixture = Fixture::new();
    let journal = fixture.journal(vec![
        JournalEvent::ModelResponded {
            turn: 1,
            response: ModelResponse::final_text("done"),
            latency_ms: 0,
        },
        ended("completed", Some("done")),
    ]);
    let mut replay = ReplayProvider::from_journal(&journal).expect("valid recording");
    replay
        .complete(fixture.request(1))
        .await
        .expect("the recorded request matches");
    assert!(replay.exhausted());

    let error = replay
        .complete(fixture.request(2))
        .await
        .expect_err("there is no second recorded request");

    assert_eq!(error.kind, ProviderErrorKind::Script);
    assert_eq!(replay.diverged_at(), Some(2));
    assert!(!replay.exhausted());
}

#[tokio::test]
async fn an_unanswered_recorded_request_must_be_reached() {
    let fixture = Fixture::new();
    // A failed session may end while its last request is unanswered.
    let journal = fixture.journal(vec![ended("deadline", None)]);
    let mut replay = ReplayProvider::from_journal(&journal).expect("valid recording");
    assert!(
        !replay.exhausted(),
        "the unanswered request is still to be replayed"
    );

    let error = replay
        .complete(fixture.request(1))
        .await
        .expect_err("the recording has no answer");

    assert_eq!(error.kind, ProviderErrorKind::Script);
    assert!(error.message.contains("turn 1"), "{}", error.message);
    assert_eq!(replay.diverged_at(), None, "the request itself matched");
    assert!(replay.exhausted());

    // A different request at that turn is a divergence, not an account.
    let mut replay = ReplayProvider::from_journal(&journal).expect("valid recording");
    let mut other = Fixture::new();
    other.system = "another system".to_string();
    let error = replay
        .complete(other.request(1))
        .await
        .expect_err("a different request");
    assert_eq!(error.kind, ProviderErrorKind::ReplayDiverged);
    assert_eq!(replay.diverged_at(), Some(1));
    assert!(!replay.exhausted());
}
