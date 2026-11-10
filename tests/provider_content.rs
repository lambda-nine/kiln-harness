//! Opaque provider content in the provider-neutral turn model: echoed
//! verbatim by the engine and the grammar, counted by the byte budget but
//! not the token floor, and absent from older journals.
mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{FakeApi, short_lifecycle_config};
use kiln_harness::audit::{AuditEvent, MemoryAudit};
use kiln_harness::engine::{Harness, HarnessError, SessionTask};
use kiln_harness::journal::{
    Grammar, JournalEvent, JournalRecord, JsonlJournal, MemoryJournal, read_journal, request_sha256,
};
use kiln_harness::provider::{
    Message, ModelRequest, ModelResponse, Provider, ProviderContent, ProviderError, ProviderInfo,
    StopReason, ToolCall, Usage,
};
use kiln_harness::replay::ReplayProvider;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const P2_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/openai-compat-p2.jsonl"
);

fn usage(output_tokens: u64) -> Usage {
    Usage {
        input_tokens: 50,
        output_tokens,
        ..Usage::default()
    }
}

fn opaque(blocks: Value) -> ProviderContent {
    ProviderContent {
        provider: "anthropic".to_string(),
        model: Some("claude-served-1".to_string()),
        blocks,
        stop_details: None,
    }
}

fn thinking_turn() -> ModelResponse {
    let blocks = json!([
        {"type": "thinking", "thinking": "", "signature": "c2lnbmF0dXJl".repeat(128)},
        {"type": "text", "text": "Writing."},
        {"type": "tool_use", "id": "toolu_01", "name": "write_file",
         "input": {"path": "a.txt", "content": "hi\n"}},
        {"type": "future_block", "z": 1, "a": [3, 2, 1]}
    ]);
    ModelResponse {
        text: Some("Writing.".to_string()),
        tool_calls: vec![ToolCall::new(
            "toolu_01",
            "write_file",
            json!({"path": "a.txt", "content": "hi\n"}),
        )],
        stop_reason: StopReason::ToolUse,
        usage: usage(20),
        provider_content: Some(opaque(blocks)),
    }
}

/// Answers from a script and keeps every request's messages.
struct Capturing {
    responses: Vec<ModelResponse>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl Provider for Capturing {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "anthropic".to_string(),
            model: "claude-requested".to_string(),
        }
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.seen
            .lock()
            .expect("seen")
            .push(request.messages.to_vec());
        Ok(self.responses.remove(0))
    }
}

fn task() -> SessionTask {
    SessionTask {
        system: "Use the tools.".to_string(),
        prompt: "Write a.txt.".to_string(),
    }
}

#[test]
fn a_piece_two_journal_verifies_and_reserializes_byte_for_byte() {
    let text = std::fs::read_to_string(P2_FIXTURE).expect("fixture");
    let journal = read_journal(std::path::Path::new(P2_FIXTURE)).expect("still verifies");
    assert!(journal.complete);
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), journal.records.len());
    for line in lines {
        let record: JournalRecord = serde_json::from_str(line).expect("record");
        assert!(!line.contains("provider_content"));
        assert_eq!(
            serde_json::to_string(&record).expect("encode"),
            line,
            "a record without provider content must encode exactly as before"
        );
    }
}

#[tokio::test]
async fn a_piece_two_journal_replays_unchanged() {
    let journal = read_journal(std::path::Path::new(P2_FIXTURE)).expect("verifies");
    let replay = ReplayProvider::from_journal(&journal).expect("replayable");
    let mut config = short_lifecycle_config();
    config.apply_limits(&replay.limits());
    let replay_task = replay.task();
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        replay,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let outcome = harness
        .run(&replay_task, CancellationToken::new())
        .await
        .expect("replay completes");
    assert_eq!(outcome.final_answer, "a.txt contains hello.");
    assert_eq!(harness.provider().diverged_at(), None);
    assert!(harness.provider().exhausted(), "reproduced");
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
}

#[test]
fn absent_provider_content_leaves_messages_and_hashes_unchanged() {
    let without = Message::Assistant {
        text: Some("t".to_string()),
        tool_calls: vec![ToolCall::new("c", "exec", json!({"argv": ["/bin/true"]}))],
        provider_content: None,
    };
    let encoded = serde_json::to_value(&without).expect("encode");
    assert_eq!(
        encoded,
        json!({"role": "assistant", "text": "t",
               "tool_calls": [{"call_id": "c", "tool": "exec", "arguments": {"argv": ["/bin/true"]}}]})
    );
    // An older record without the key decodes to None.
    let decoded: Message = serde_json::from_value(encoded).expect("decode");
    assert_eq!(decoded, without);

    let with = Message::Assistant {
        text: Some("t".to_string()),
        tool_calls: vec![ToolCall::new("c", "exec", json!({"argv": ["/bin/true"]}))],
        provider_content: Some(opaque(json!([{"type": "text", "text": "t"}]))),
    };
    let hash = |messages: &[Message]| {
        request_sha256(&ModelRequest {
            session_id: "",
            turn: 2,
            system: "s",
            tools: &[],
            messages,
            max_output_tokens: 8,
        })
    };
    assert_ne!(
        hash(std::slice::from_ref(&without)),
        hash(std::slice::from_ref(&with)),
        "the request hash covers provider content"
    );
    let round_trip: Message =
        serde_json::from_str(&serde_json::to_string(&with).expect("encode")).expect("decode");
    assert_eq!(round_trip, with);
}

#[test]
fn opaque_bytes_count_toward_the_budget_but_not_the_token_floor() {
    let response = thinking_turn();
    let neutral = ModelResponse {
        provider_content: None,
        ..response.clone()
    };
    assert_eq!(response.token_floor_bytes(), neutral.output_bytes());
    let content = response.provider_content.as_ref().expect("content");
    let blocks = serde_json::to_vec(&content.blocks).expect("encode").len() as u64;
    let model = content.model.as_ref().map_or(0, String::len) as u64;
    assert_eq!(content.bytes(), blocks + model);
    assert_eq!(
        response.output_bytes(),
        response.token_floor_bytes() + blocks + model
    );
    assert!(blocks > 1000, "the signature dominates");

    let mut refused = opaque(json!([]));
    refused.stop_details = Some(json!({"type": "refusal", "category": "cyber"}));
    let details = serde_json::to_vec(refused.stop_details.as_ref().expect("details"))
        .expect("encode")
        .len() as u64;
    assert_eq!(refused.bytes(), 2 + model + details);
}

#[tokio::test]
async fn the_engine_echoes_provider_content_and_the_journal_rebuilds_it() {
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("opaque.jsonl");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let first = thinking_turn();
    let mut last = ModelResponse::final_text("Wrote a.txt.");
    last.usage = usage(5);
    last.provider_content = Some(opaque(json!([{"type": "text", "text": "Wrote a.txt."}])));
    let provider = Capturing {
        responses: vec![first.clone(), last],
        seen: seen.clone(),
    };
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        short_lifecycle_config(),
    );

    let outcome = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("session completes");

    assert_eq!(outcome.final_answer, "Wrote a.txt.");
    let seen = seen.lock().expect("seen").clone();
    assert_eq!(seen.len(), 2);
    let Message::Assistant {
        provider_content, ..
    } = &seen[1][1]
    else {
        panic!("second request carries the assistant turn: {:?}", seen[1]);
    };
    assert_eq!(provider_content, &first.provider_content, "unmodified");
    // The grammar rebuilds the same history, provider content included,
    // or the request hashes would not verify.
    let journal = read_journal(&path).expect("journal verifies");
    assert!(journal.complete);
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
    // An edited block changes the rebuilt history, so the next recorded
    // request hash no longer matches.
    let mut grammar = Grammar::default();
    let mut refused = None;
    for record in &journal.records {
        let mut event = record.event.clone();
        if let JournalEvent::ModelResponded {
            turn: 1, response, ..
        } = &mut event
        {
            let content = response.provider_content.as_mut().expect("content");
            content.blocks[0]["signature"] = json!("edited");
        }
        if let Err(error) = grammar.feed(record.sequence, &event) {
            refused = Some(error.to_string());
            break;
        }
    }
    let refused = refused.expect("an edited block is refused");
    assert!(refused.contains("hash differs"), "{refused}");

    let served = harness
        .audit()
        .records
        .iter()
        .filter_map(|record| match &record.event {
            AuditEvent::ModelResponded { served_model, .. } => Some(served_model.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(served, vec![Some("claude-served-1".to_string()); 2]);
}

#[tokio::test]
async fn opaque_content_is_held_to_the_session_byte_budget() {
    let provider = Capturing {
        responses: vec![thinking_turn(), ModelResponse::final_text("never")],
        seen: Arc::default(),
    };
    let mut config = short_lifecycle_config();
    let floor = thinking_turn().token_floor_bytes();
    // Enough for the neutral view, not for the signature.
    config.max_total_model_output_bytes = floor + 100;
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("over the byte budget");
    assert!(
        matches!(error, HarnessError::OutputBytesBudget(_)),
        "{error}"
    );
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
}
