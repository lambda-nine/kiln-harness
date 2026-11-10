//! The journal event grammar, enforced identically by writers and readers.
mod common;

use common::{chained_lines, write_journal_lines};
use kiln_harness::engine::HarnessConfig;
use kiln_harness::journal::request_sha256;
use kiln_harness::journal::{
    JOURNAL_SCHEMA, JournalError, JournalEvent, JournalSink, LoadedJournal, MemoryJournal,
    read_journal,
};
use kiln_harness::provider::{
    Message, ModelRequest, ModelResponse, ProviderErrorKind, ProviderInfo, ToolCall,
    ToolDefinition, ToolResult, Usage,
};
use kiln_harness::replay::ReplayProvider;
use kiln_harness::tools::{ToolLimits, ToolRegistry};
use serde_json::json;

const SESSION: &str = "hs-grammar";

fn usage(input_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        output_tokens: 1,
        ..Usage::default()
    }
}

fn started() -> JournalEvent {
    JournalEvent::SessionStarted {
        schema: JOURNAL_SCHEMA,
        harness_version: "0.1.0".to_string(),
        provider: ProviderInfo {
            name: "test".to_string(),
            model: "test".to_string(),
        },
        system: "system".to_string(),
        task: "task".to_string(),
        tools: tools(),
        limits: HarnessConfig::default().limits(),
    }
}

fn tools() -> Vec<ToolDefinition> {
    ToolRegistry::new(ToolLimits::default())
        .definitions()
        .to_vec()
}

fn diff_call(call_id: &str) -> ToolCall {
    ToolCall::new(call_id, "workspace_diff", json!({}))
}

/// The history the engine sends in `turn` of the valid session below.
fn history(turn: u32) -> Vec<Message> {
    let mut messages = vec![Message::User {
        text: "task".to_string(),
    }];
    if turn >= 2 {
        messages.push(Message::Assistant {
            text: None,
            tool_calls: vec![diff_call("a"), diff_call("b")],
        });
        messages.push(Message::ToolResults {
            results: ["a", "b"]
                .map(|call_id| ToolResult {
                    call_id: call_id.to_string(),
                    tool: "workspace_diff".to_string(),
                    result: json!({"ok": true}),
                })
                .to_vec(),
        });
    }
    messages
}

fn ready() -> JournalEvent {
    JournalEvent::SandboxReady {
        sandbox_id: "sb-1".to_string(),
        baseline_file_count: 0,
    }
}

fn requested(turn: u32) -> JournalEvent {
    let messages = history(turn);
    let tools = tools();
    JournalEvent::ModelRequested {
        turn,
        request_sha256: request_sha256(&ModelRequest {
            session_id: SESSION,
            turn,
            system: "system",
            tools: &tools,
            messages: &messages,
            max_output_tokens: HarnessConfig::default().max_output_tokens_per_request,
        }),
        message_count: messages.len(),
    }
}

fn responded(turn: u32, response: ModelResponse) -> JournalEvent {
    JournalEvent::ModelResponded {
        turn,
        response,
        latency_ms: 0,
    }
}

fn two_calls() -> ModelResponse {
    ModelResponse {
        usage: usage(10),
        ..ModelResponse::tool_use([diff_call("a"), diff_call("b")])
    }
}

fn final_response() -> ModelResponse {
    ModelResponse {
        usage: usage(20),
        ..ModelResponse::final_text("done")
    }
}

fn executed(turn: u32, index: u32, call_id: &str) -> JournalEvent {
    JournalEvent::ToolExecuted {
        turn,
        index,
        audit_call_id: format!("call-{turn}-{index}"),
        call: diff_call(call_id),
        ok: true,
        result: json!({"ok": true}),
        execution_ids: vec![format!("ex-{index}")],
    }
}

struct Ended {
    outcome: &'static str,
    work_outcome: &'static str,
    final_answer: Option<&'static str>,
    steps: u32,
    tool_calls: u32,
    usage: Usage,
    sandbox_deleted: bool,
}

impl Ended {
    fn completed() -> Self {
        Self {
            outcome: "completed",
            work_outcome: "completed",
            final_answer: Some("done"),
            steps: 2,
            tool_calls: 2,
            usage: usage(10) + usage(20),
            sandbox_deleted: true,
        }
    }

    fn event(&self) -> JournalEvent {
        JournalEvent::SessionEnded {
            outcome: self.outcome.to_string(),
            work_outcome: self.work_outcome.to_string(),
            final_answer: self.final_answer.map(str::to_string),
            steps: self.steps,
            tool_calls: self.tool_calls,
            usage: self.usage,
            sandbox_deleted: self.sandbox_deleted,
        }
    }
}

/// What the engine writes for a two-call turn followed by a final answer.
fn valid() -> Vec<JournalEvent> {
    vec![
        started(),
        ready(),
        requested(1),
        responded(1, two_calls()),
        executed(1, 1, "a"),
        executed(1, 2, "b"),
        requested(2),
        responded(2, final_response()),
        Ended::completed().event(),
    ]
}

fn write_all(events: &[JournalEvent]) -> Result<(), JournalError> {
    let mut journal = MemoryJournal::default();
    for event in events {
        journal.append(SESSION, event.clone())?;
    }
    Ok(())
}

fn read_all(events: Vec<JournalEvent>) -> Result<LoadedJournal, JournalError> {
    let temp = tempfile::tempdir().expect("tempdir");
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

fn assert_rejected(name: &str, events: Vec<JournalEvent>) {
    let written = write_all(&events);
    assert!(
        matches!(written, Err(JournalError::Grammar { .. })),
        "{name}: writer gave {written:?}"
    );
    let read = read_all(events.clone());
    assert!(
        matches!(read, Err(JournalError::Grammar { .. })),
        "{name}: reader gave {read:?}"
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

#[test]
fn engine_shaped_journals_are_accepted() {
    assert_accepted("tool turn then final answer", valid());

    let mut pending = valid();
    pending.truncate(7);
    pending.push(
        Ended {
            outcome: "deadline",
            work_outcome: "deadline",
            final_answer: None,
            usage: usage(10),
            ..Ended::completed()
        }
        .event(),
    );
    assert_accepted("unanswered request after a deadline", pending);

    let mut partial = valid();
    partial.truncate(5);
    partial.push(
        Ended {
            outcome: "failed",
            work_outcome: "failed",
            final_answer: None,
            steps: 1,
            tool_calls: 1,
            usage: usage(10),
            sandbox_deleted: true,
        }
        .event(),
    );
    assert_accepted("fewer calls than requested, then the end", partial);

    let mut cleanup = valid();
    cleanup.pop();
    cleanup.push(
        Ended {
            outcome: "cleanup_failed",
            sandbox_deleted: false,
            ..Ended::completed()
        }
        .event(),
    );
    assert_accepted("completed work with unconfirmed cleanup", cleanup);

    assert_accepted(
        "create failure",
        vec![
            started(),
            Ended {
                outcome: "create_failed",
                work_outcome: "create_failed",
                final_answer: None,
                steps: 0,
                tool_calls: 0,
                usage: Usage::default(),
                sandbox_deleted: true,
            }
            .event(),
        ],
    );

    assert_accepted(
        "provider failure",
        vec![
            started(),
            ready(),
            requested(1),
            JournalEvent::ModelFailed {
                turn: 1,
                kind: ProviderErrorKind::Script,
                message: "boom".to_string(),
            },
            Ended {
                outcome: "failed",
                work_outcome: "failed",
                final_answer: None,
                steps: 1,
                tool_calls: 0,
                usage: Usage::default(),
                sandbox_deleted: true,
            }
            .event(),
        ],
    );
}

#[test]
fn tool_records_must_be_the_sanitized_response_calls() {
    // The engine records unknown and oversized calls as sanitized.
    let oversized = json!({"content": "x".repeat(70 << 10)});
    let response = ModelResponse::tool_use([
        ToolCall::new("u", "Not A Tool", json!({})),
        ToolCall::new("o", "write_file", oversized.clone()),
    ]);
    let sanitized = [
        ToolCall::new("u", "unknown", json!({})),
        ToolCall::new(
            "o",
            "write_file",
            json!({"omitted": "arguments exceeded byte limit"}),
        ),
    ];
    let tool = |index: u32, call: &ToolCall| JournalEvent::ToolExecuted {
        turn: 1,
        index,
        audit_call_id: format!("call-1-{index}"),
        call: call.clone(),
        ok: false,
        result: json!({"ok": false, "error": "rejected"}),
        execution_ids: Vec::new(),
    };
    let ended = Ended {
        outcome: "failed",
        work_outcome: "failed",
        final_answer: None,
        steps: 1,
        tool_calls: 2,
        usage: Usage::default(),
        sandbox_deleted: true,
    }
    .event();
    let mut events = vec![started(), ready(), requested(1), responded(1, response)];
    events.extend([tool(1, &sanitized[0]), tool(2, &sanitized[1]), ended]);
    assert_accepted("sanitized unknown and oversized calls", events.clone());
    events[5] = tool(2, &ToolCall::new("o", "write_file", oversized));
    assert_rejected("oversized arguments kept raw", events);
}

#[test]
#[allow(clippy::too_many_lines)]
fn out_of_order_and_inconsistent_journals_are_rejected() {
    type Mutation = fn(&mut Vec<JournalEvent>);
    let cases: [(&str, Mutation); 30] = [
        ("tool before sandbox_ready", |events| {
            events.insert(1, executed(99, 7, "ghost"));
        }),
        ("second sandbox_ready", |events| {
            events.insert(2, ready());
        }),
        ("request without sandbox_ready", |events| {
            events.remove(1);
        }),
        ("tool before its response", |events| {
            events.swap(3, 4);
        }),
        ("turns that skip", |events| events[2] = requested(2)),
        ("two requests in a row", |events| {
            events.insert(3, requested(2));
        }),
        ("response for another turn", |events| {
            events[3] = responded(3, two_calls());
        }),
        ("tool index out of order", |events| {
            events[4] = executed(1, 2, "a");
        }),
        ("tool call ID differs from the response", |events| {
            events[4] = executed(1, 1, "zzz");
        }),
        ("more tools than calls", |events| {
            events.insert(6, executed(1, 3, "c"));
        }),
        ("next request before every call ran", |events| {
            events.remove(5);
        }),
        ("request after the final response", |events| {
            let end = events.pop().expect("end");
            events.push(requested(3));
            events.push(end);
        }),
        ("tool for an older turn", |events| {
            events.insert(8, executed(1, 3, "a"));
        }),
        ("wrong tool-call count", |events| {
            *events.last_mut().expect("end") = Ended {
                tool_calls: 1,
                ..Ended::completed()
            }
            .event();
        }),
        ("wrong steps", |events| {
            *events.last_mut().expect("end") = Ended {
                steps: 3,
                ..Ended::completed()
            }
            .event();
        }),
        ("wrong usage", |events| {
            *events.last_mut().expect("end") = Ended {
                usage: Usage::default(),
                ..Ended::completed()
            }
            .event();
        }),
        ("final answer differs", |events| {
            *events.last_mut().expect("end") = Ended {
                final_answer: Some("something else"),
                ..Ended::completed()
            }
            .event();
        }),
        ("completed without confirmed deletion", |events| {
            *events.last_mut().expect("end") = Ended {
                sandbox_deleted: false,
                ..Ended::completed()
            }
            .event();
        }),
        (
            "unconfirmed deletion that is not cleanup_failed",
            |events| {
                events.truncate(1);
                events.push(
                    Ended {
                        outcome: "create_failed",
                        work_outcome: "create_failed",
                        final_answer: None,
                        steps: 0,
                        tool_calls: 0,
                        usage: Usage::default(),
                        sandbox_deleted: false,
                    }
                    .event(),
                );
            },
        ),
        ("cleanup_failed with confirmed deletion", |events| {
            *events.last_mut().expect("end") = Ended {
                outcome: "cleanup_failed",
                ..Ended::completed()
            }
            .event();
        }),
        ("tool call differs from the model's call", |events| {
            let JournalEvent::ToolExecuted { call, result, .. } = &mut events[4] else {
                panic!("tool record");
            };
            *call = ToolCall::new("a", "exec", json!({"argv": ["id"]}));
            *result = json!({"ok": true, "stdout": "fabricated"});
        }),
        ("tool arguments differ from the model's call", |events| {
            let JournalEvent::ToolExecuted { call, .. } = &mut events[4] else {
                panic!("tool record");
            };
            call.arguments = json!({"extra": true});
        }),
        (
            "tool result differs from what the next request carried",
            |events| {
                let JournalEvent::ToolExecuted { result, .. } = &mut events[5] else {
                    panic!("tool record");
                };
                *result = json!({"ok": true, "added": ["fabricated.txt"]});
            },
        ),
        ("message_count differs from the history", |events| {
            let JournalEvent::ModelRequested { message_count, .. } = &mut events[6] else {
                panic!("request record");
            };
            *message_count = 999;
        }),
        ("request hash differs from the history", |events| {
            let JournalEvent::ModelRequested { request_sha256, .. } = &mut events[2] else {
                panic!("request record");
            };
            *request_sha256 = "0".repeat(64);
        }),
        ("outcome differs from the work outcome", |events| {
            *events.last_mut().expect("end") = Ended {
                outcome: "deadline",
                ..Ended::completed()
            }
            .event();
        }),
        ("unknown work outcome", |events| {
            *events.last_mut().expect("end") = Ended {
                outcome: "great",
                work_outcome: "great",
                ..Ended::completed()
            }
            .event();
        }),
        ("final answer without completed work", |events| {
            *events.last_mut().expect("end") = Ended {
                outcome: "failed",
                work_outcome: "failed",
                ..Ended::completed()
            }
            .event();
        }),
        ("completed with an unanswered request", |events| {
            events.remove(7);
            *events.last_mut().expect("end") = Ended {
                usage: usage(10),
                ..Ended::completed()
            }
            .event();
        }),
        ("request after a provider failure", |events| {
            events.truncate(3);
            events.push(JournalEvent::ModelFailed {
                turn: 1,
                kind: ProviderErrorKind::Script,
                message: "boom".to_string(),
            });
            events.push(requested(2));
        }),
    ];
    for (name, mutate) in cases {
        let mut events = valid();
        mutate(&mut events);
        assert_rejected(name, events);
    }
}
