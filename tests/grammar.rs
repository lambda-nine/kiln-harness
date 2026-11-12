//! The journal event grammar, enforced identically by writers and readers.
mod common;

use common::{chained_lines, write_journal_lines};
use kiln_harness::engine::CheckOutcome;
use kiln_harness::engine::HarnessConfig;
use kiln_harness::journal::request_sha256;
use kiln_harness::journal::{
    JOURNAL_SCHEMA, JournalError, JournalEvent, JournalSink, LoadedJournal, MemoryJournal,
    RecordedCheck, RecordedSeedFile, base64_decode, base64_encode, read_journal,
};
use kiln_harness::provider::{
    Message, ModelRequest, ModelResponse, ProviderErrorKind, ProviderInfo, ToolCall,
    ToolDefinition, ToolResult, Usage,
};
use kiln_harness::replay::ReplayProvider;
use kiln_harness::tools::{CHECK_OUTPUT_BYTES, ToolLimits, ToolRegistry};
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
        seed: Vec::new(),
        check: None,
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
            provider_content: None,
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
        expiry_lifetime_ms: None,
        expiry_idle_ms: None,
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
            truncation_recoveries: 0,
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

/// Like `assert_rejected`, and the writer's and reader's reason must
/// mention `reason`, so the rule under test is the one that fired.
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
    assert_rejected(name, events);
}

fn with_limits(
    mut events: Vec<JournalEvent>,
    change: impl FnOnce(&mut kiln_harness::journal::SessionLimits),
) -> Vec<JournalEvent> {
    let JournalEvent::SessionStarted { limits, .. } = &mut events[0] else {
        panic!("session start");
    };
    change(limits);
    events
}

#[test]
fn recorded_limits_bound_what_may_follow() {
    assert_rejected_because(
        "a second turn under max_steps 1",
        with_limits(valid(), |limits| limits.max_steps = 1),
        "beyond max_steps",
    );
    assert_rejected_because(
        "a second tool call under max_tool_calls 1",
        with_limits(valid(), |limits| limits.max_tool_calls = 1),
        "beyond max_tool_calls",
    );
    assert_rejected_because(
        "two calls in a turn under max_tool_calls_per_turn 1",
        with_limits(valid(), |limits| limits.max_tool_calls_per_turn = 1),
        "after a response the engine refuses",
    );
    assert_rejected_because(
        "tools after the input budget was exceeded",
        with_limits(valid(), |limits| limits.max_total_input_tokens = 5),
        "after a response the engine refuses",
    );
    assert_rejected_because(
        "completion after the output budget was exceeded",
        with_limits(valid(), |limits| limits.max_total_output_tokens = 1),
        "completed after a response the engine refuses",
    );
    assert_rejected_because(
        "completion after the model-output byte budget was exceeded",
        with_limits(valid(), |limits| {
            limits.max_total_model_output_bytes = valid_output_bytes() - 1;
        }),
        "completed after a response the engine refuses",
    );
    assert_rejected_because(
        "completion with a final answer over its byte bound",
        with_limits(valid(), |limits| limits.max_final_answer_bytes = 3),
        "completed after a response the engine refuses",
    );

    // One response over the per-request output cap.
    let mut over_cap = valid();
    let JournalEvent::ModelResponded { response, .. } = &mut over_cap[3] else {
        panic!("response");
    };
    response.usage.output_tokens = 5000;
    let JournalEvent::SessionEnded { usage, .. } = over_cap.last_mut().expect("end") else {
        panic!("end");
    };
    usage.output_tokens = 5001;
    assert_rejected_because(
        "tools after a response over the per-request cap",
        over_cap,
        "after a response the engine refuses",
    );

    // The same journals are fine when the limits allow them.
    assert_accepted(
        "exactly at the limits",
        with_limits(valid(), |limits| {
            limits.max_steps = 2;
            limits.max_tool_calls = 2;
            limits.max_tool_calls_per_turn = 2;
            limits.max_total_input_tokens = 30;
            limits.max_total_output_tokens = 2;
            limits.max_final_answer_bytes = 4;
            limits.max_total_model_output_bytes = valid_output_bytes();
        }),
    );
}

#[test]
fn tool_names_and_ids_count_toward_the_recorded_byte_budget() {
    // A budget that covers the text and arguments alone, but not the call
    // IDs and tool names as well, refuses the continuation.
    let without_names: u64 = valid()
        .iter()
        .filter_map(|event| match event {
            JournalEvent::ModelResponded { response, .. } => {
                let mut stripped = response.clone();
                for call in &mut stripped.tool_calls {
                    call.call_id.clear();
                    call.tool.clear();
                }
                Some(stripped.output_bytes())
            }
            _ => None,
        })
        .sum();
    assert!(without_names < valid_output_bytes());
    assert_rejected_because(
        "names and IDs push the session over its byte budget",
        with_limits(valid(), |limits| {
            limits.max_total_model_output_bytes = without_names;
        }),
        "after a response the engine refuses",
    );
}

/// The model output the `valid()` journal's responses add up to.
fn valid_output_bytes() -> u64 {
    valid()
        .iter()
        .filter_map(|event| match event {
            JournalEvent::ModelResponded { response, .. } => Some(response.output_bytes()),
            _ => None,
        })
        .sum()
}

/// The engine's own journal for a refused response, from a real run.
async fn engine_events(responses: Vec<ModelResponse>, config: HarnessConfig) -> Vec<JournalEvent> {
    let mut harness = kiln_harness::engine::Harness::new(
        common::FakeApi::default(),
        kiln_harness::provider::ScriptedProvider::new(responses),
        kiln_harness::audit::MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let _ = harness
        .run(
            &kiln_harness::engine::SessionTask::demo(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    harness
        .journal()
        .records
        .iter()
        .map(|record| record.event.clone())
        .collect()
}

/// Continue an engine journal past its refused response: run the first call.
fn forge_tool_after(mut events: Vec<JournalEvent>) -> Vec<JournalEvent> {
    let Some(JournalEvent::SessionEnded {
        tool_calls,
        steps,
        usage,
        ..
    }) = events.pop()
    else {
        panic!("the engine journal ends the session");
    };
    let JournalEvent::SessionStarted { tools, limits, .. } = &events[0] else {
        panic!("session start");
    };
    let (tools, limits) = (tools.clone(), *limits);
    let Some(JournalEvent::ModelResponded { turn, response, .. }) = events.last().cloned() else {
        panic!("the refused response is the last record");
    };
    let call = kiln_harness::tools::sanitize_call(&response.tool_calls[0], &tools, &limits);
    events.push(JournalEvent::ToolExecuted {
        turn,
        index: 1,
        audit_call_id: format!("call-{turn}-1"),
        call,
        ok: true,
        result: json!({"ok": true}),
        execution_ids: Vec::new(),
    });
    events.push(JournalEvent::SessionEnded {
        outcome: "failed".to_string(),
        work_outcome: "failed".to_string(),
        final_answer: None,
        steps,
        tool_calls: tool_calls + 1,
        usage,
        sandbox_deleted: true,
        truncation_recoveries: 0,
    });
    events
}

/// Turn an engine journal ending in a refused final response into a
/// completed one.
fn forge_completion(mut events: Vec<JournalEvent>) -> Vec<JournalEvent> {
    let Some(JournalEvent::SessionEnded {
        tool_calls,
        steps,
        usage,
        truncation_recoveries,
        ..
    }) = events.pop()
    else {
        panic!("the engine journal ends the session");
    };
    let Some(JournalEvent::ModelResponded { response, .. }) = events.last().cloned() else {
        panic!("the refused response is the last record");
    };
    events.push(JournalEvent::SessionEnded {
        outcome: "completed".to_string(),
        work_outcome: "completed".to_string(),
        final_answer: response.text,
        steps,
        tool_calls,
        usage,
        sandbox_deleted: true,
        truncation_recoveries,
    });
    events
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn engine_refusals_verify_and_forged_continuations_do_not() {
    use kiln_harness::provider::StopReason;
    let short = common::short_lifecycle_config;
    let diff = |id: &str| ToolCall::new(id, "workspace_diff", json!({}));

    let refused_tool_turns = [
        (
            "assistant text over its bound",
            vec![ModelResponse {
                text: Some("much too long".to_string()),
                ..ModelResponse::tool_use([diff("a")])
            }],
            HarnessConfig {
                max_final_answer_bytes: 4,
                ..short()
            },
        ),
        (
            "truncated tool turn",
            vec![ModelResponse {
                stop_reason: StopReason::MaxTokens,
                ..ModelResponse::tool_use([diff("a")])
            }],
            HarnessConfig {
                max_truncation_recoveries: 0,
                ..short()
            },
        ),
        (
            "invalid call ID",
            vec![ModelResponse::tool_use([diff("not valid!")])],
            short(),
        ),
        (
            "repeated call ID",
            vec![ModelResponse::tool_use([diff("a"), diff("a")])],
            short(),
        ),
        (
            "over the per-request output cap",
            vec![ModelResponse {
                usage: Usage {
                    output_tokens: 9,
                    ..Usage::default()
                },
                ..ModelResponse::tool_use([diff("a")])
            }],
            HarnessConfig {
                max_output_tokens_per_request: 8,
                ..short()
            },
        ),
    ];
    for (name, responses, config) in refused_tool_turns {
        let events = engine_events(responses, config).await;
        assert_accepted(name, events.clone());
        assert_rejected_because(
            name,
            forge_tool_after(events),
            "after a response the engine refuses",
        );
    }

    let refused_final_answers = [
        (
            "final answer over its bound",
            vec![ModelResponse::final_text("much too long")],
            HarnessConfig {
                max_final_answer_bytes: 4,
                ..short()
            },
        ),
        (
            "refusal",
            vec![ModelResponse {
                stop_reason: StopReason::Refusal,
                ..ModelResponse::final_text("no")
            }],
            short(),
        ),
        (
            "truncated final answer",
            vec![ModelResponse {
                stop_reason: StopReason::MaxTokens,
                ..ModelResponse::final_text("half")
            }],
            HarnessConfig {
                max_truncation_recoveries: 0,
                ..short()
            },
        ),
        (
            "tool_use stop with no tool calls",
            vec![ModelResponse {
                stop_reason: StopReason::ToolUse,
                ..ModelResponse::final_text("done")
            }],
            short(),
        ),
        (
            "over the total output budget",
            vec![ModelResponse {
                usage: Usage {
                    output_tokens: 11,
                    ..Usage::default()
                },
                ..ModelResponse::final_text("done")
            }],
            HarnessConfig {
                max_total_output_tokens: 10,
                ..short()
            },
        ),
    ];
    for (name, responses, config) in refused_final_answers {
        let events = engine_events(responses, config).await;
        assert_accepted(name, events.clone());
        assert_rejected_because(
            name,
            forge_completion(events),
            "completed after a response the engine refuses",
        );
    }

    // The step and call budgets, from real runs: the journals verify and a
    // further request or call is refused.
    let step = engine_events(
        vec![
            ModelResponse::tool_use([diff("a")]),
            ModelResponse::final_text("done"),
        ],
        HarnessConfig {
            max_steps: 1,
            ..short()
        },
    )
    .await;
    assert_accepted("step budget", step);
    let calls = engine_events(
        vec![ModelResponse::tool_use([diff("a"), diff("b")])],
        HarnessConfig {
            max_tool_calls: 1,
            ..short()
        },
    )
    .await;
    assert_accepted("call budget", calls.clone());
    let mut over_calls = calls;
    let Some(JournalEvent::SessionEnded {
        tool_calls,
        steps,
        usage,
        ..
    }) = over_calls.pop()
    else {
        panic!("end");
    };
    over_calls.push(JournalEvent::ToolExecuted {
        turn: 1,
        index: 2,
        audit_call_id: "call-1-2".to_string(),
        call: diff("b"),
        ok: true,
        result: json!({"ok": true}),
        execution_ids: Vec::new(),
    });
    over_calls.push(JournalEvent::SessionEnded {
        outcome: "failed".to_string(),
        work_outcome: "failed".to_string(),
        final_answer: None,
        steps,
        tool_calls: tool_calls + 1,
        usage,
        sandbox_deleted: true,
        truncation_recoveries: 0,
    });
    assert_rejected_because("past the call budget", over_calls, "beyond max_tool_calls");
}

#[test]
fn recorded_tasks_must_be_ones_the_engine_accepts() {
    use kiln_harness::engine::MAX_TASK_TEXT_BYTES;

    let over = "x".repeat(MAX_TASK_TEXT_BYTES + 1);
    for (name, system, task) in [
        ("empty prompt", "system".to_string(), String::new()),
        ("prompt one byte over", "system".to_string(), over.clone()),
        ("system one byte over", over.clone(), "task".to_string()),
    ] {
        let mut events = valid();
        let JournalEvent::SessionStarted {
            system: recorded_system,
            task: recorded_task,
            ..
        } = &mut events[0]
        else {
            panic!("session start");
        };
        *recorded_system = system;
        *recorded_task = task;
        // Only the start record is needed: it is refused on its own.
        events.truncate(1);
        assert_rejected_because(name, events, "recorded task is invalid");
    }

    // Exactly at the limit is fine for both strings.
    let at = "x".repeat(MAX_TASK_TEXT_BYTES);
    let mut events = vec![started()];
    let JournalEvent::SessionStarted { system, task, .. } = &mut events[0] else {
        panic!("session start");
    };
    *system = at.clone();
    *task = at;
    write_all(&events).expect("a task at the limit is valid");
}

/// The valid session with a recorded seed and check, and the check's
/// result after the final answer.
fn checked() -> Vec<JournalEvent> {
    let mut events = valid();
    set_task(
        &mut events,
        vec![seed_file("a/b.txt", b"seeded\n"), seed_file("c.txt", b"")],
        Some(check("exit 0\n", 15_000)),
    );
    let end = events.pop().expect("session_ended");
    events.push(check_completed(CheckOutcome::Passed));
    events.push(end);
    events
}

fn seed_file(path: &str, contents: &[u8]) -> RecordedSeedFile {
    RecordedSeedFile {
        path: path.to_string(),
        contents_b64: base64_encode(contents),
    }
}

fn check(script: &str, timeout_ms: u64) -> RecordedCheck {
    RecordedCheck {
        script_b64: base64_encode(script.as_bytes()),
        timeout_ms,
    }
}

fn check_completed(outcome: CheckOutcome) -> JournalEvent {
    JournalEvent::CheckCompleted {
        outcome,
        execution_id: Some("ex-9".to_string()),
        stdout: String::new(),
        stderr: String::new(),
    }
}

fn set_task(
    events: &mut [JournalEvent],
    new_seed: Vec<RecordedSeedFile>,
    new_check: Option<RecordedCheck>,
) {
    let JournalEvent::SessionStarted { seed, check, .. } = &mut events[0] else {
        panic!("session start");
    };
    *seed = new_seed;
    *check = new_check;
}

#[test]
fn a_recorded_check_completes_once_after_the_final_answer() {
    assert_accepted("seed, check and its result", checked());
    for outcome in [
        CheckOutcome::Failed {
            exit_code: Some(1),
            timed_out: false,
        },
        CheckOutcome::Error {
            reason: "deadline".to_string(),
        },
    ] {
        let mut events = checked();
        let at = events.len() - 2;
        events[at] = check_completed(outcome);
        assert_accepted("a check that did not pass still completes", events);
    }

    // Work that did not complete may end without the check.
    let mut failed = checked();
    failed.truncate(5);
    failed.push(
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
    assert_accepted("failed work without a check result", failed);

    let mut missing = checked();
    missing.remove(missing.len() - 2);
    assert_rejected_because(
        "completed without the recorded check's result",
        missing,
        "needs check_completed",
    );

    let mut unrecorded = valid();
    let end = unrecorded.pop().expect("end");
    unrecorded.push(check_completed(CheckOutcome::Passed));
    unrecorded.push(end);
    assert_rejected_because(
        "a result without a recorded check",
        unrecorded,
        "without a recorded check",
    );

    let mut twice = checked();
    let at = twice.len() - 1;
    twice.insert(at, check_completed(CheckOutcome::Passed));
    assert_rejected_because("two results", twice, "second check_completed");

    // Before the final answer: after the tool turn, and after the last
    // request but before its response.
    let mut early = checked();
    early.insert(6, check_completed(CheckOutcome::Passed));
    assert_rejected_because("after a tool turn", early, "accepted final answer");
    let mut pending = checked();
    pending.insert(7, check_completed(CheckOutcome::Passed));
    assert_rejected_because(
        "while a request is pending",
        pending,
        "accepted final answer",
    );
    // Nothing but session_ended follows it.
    let mut after = checked();
    let at = after.len() - 1;
    after.insert(at, requested(3));
    assert_rejected("a request after the check", after);
}

#[test]
fn check_output_may_not_exceed_four_kib_of_utf8() {
    let with_output = |stdout: String, stderr: String| {
        let mut events = checked();
        let at = events.len() - 2;
        events[at] = JournalEvent::CheckCompleted {
            outcome: CheckOutcome::Passed,
            execution_id: Some("ex-9".to_string()),
            stdout,
            stderr,
        };
        events
    };
    assert_eq!(CHECK_OUTPUT_BYTES, 4096);
    // Bytes, not characters: 1365 replacement characters are 4095 bytes.
    let at_limit = "o".repeat(4096);
    let replaced = "\u{fffd}".repeat(1365);
    assert_accepted(
        "4096 bytes of stdout",
        with_output(at_limit.clone(), String::new()),
    );
    assert_accepted(
        "4096 bytes of stderr",
        with_output(String::new(), at_limit.clone()),
    );
    assert_accepted(
        "replacement characters within the bound",
        with_output(replaced.clone(), replaced),
    );
    let over = "o".repeat(4097);
    assert_rejected_because(
        "4097 bytes of stdout",
        with_output(over.clone(), String::new()),
        "longer than 4 KiB",
    );
    assert_rejected_because(
        "4097 bytes of stderr",
        with_output(String::new(), over),
        "longer than 4 KiB",
    );
    // 1366 replacement characters: within 4096 characters, over 4096 bytes.
    assert_rejected_because(
        "replacement characters past the bound",
        with_output(String::new(), "\u{fffd}".repeat(1366)),
        "longer than 4 KiB",
    );
}

#[test]
fn recorded_seeds_and_checks_must_be_ones_the_engine_accepts() {
    let limits = HarnessConfig::default().limits();
    let max_file = limits.tool_limits.max_file_bytes;
    let too_many = (0..=256)
        .map(|index| seed_file(&format!("f{index:04}"), b"x"))
        .collect::<Vec<_>>();
    let bad_seeds: Vec<(&str, Vec<RecordedSeedFile>)> = vec![
        ("a parent component", vec![seed_file("../x", b"")]),
        ("a dot component", vec![seed_file("a/./x", b"")]),
        ("an absolute path", vec![seed_file("/etc/passwd", b"")]),
        ("the workspace prefix", vec![seed_file("/workspace/x", b"")]),
        ("an empty component", vec![seed_file("a//x", b"")]),
        ("a NUL", vec![seed_file("a\0x", b"")]),
        ("a control character", vec![seed_file("a\nx", b"")]),
        (
            "out of order",
            vec![seed_file("b", b""), seed_file("a", b"")],
        ),
        (
            "a duplicate",
            vec![seed_file("a", b""), seed_file("a", b"")],
        ),
        (
            "a file that is also a directory",
            vec![seed_file("a", b""), seed_file("a/b", b"")],
        ),
        ("too many files", too_many),
        (
            "a file over the limit",
            vec![seed_file("big", &vec![0; max_file + 1])],
        ),
        (
            "invalid base64",
            vec![RecordedSeedFile {
                path: "x".to_string(),
                contents_b64: "not base64!".to_string(),
            }],
        ),
        (
            "non-canonical base64",
            vec![RecordedSeedFile {
                path: "x".to_string(),
                contents_b64: "QR==".to_string(),
            }],
        ),
    ];
    for (name, seed) in bad_seeds {
        let mut events = vec![started()];
        set_task(&mut events, seed, None);
        assert_rejected_because(name, events, "recorded task is invalid");
    }

    let half = limits.session_timeout_ms / 2;
    let bad_checks = [
        ("a sub-second timeout", check("exit 0", 999)),
        ("a timeout over ten minutes", check("exit 0", 600_001)),
        ("more than half the session", check("exit 0", half + 1)),
        ("an empty script", check("", 1_000)),
        (
            "a script over the stdin limit",
            check(&"x".repeat(limits.tool_limits.max_stdin_bytes + 1), 1_000),
        ),
        (
            "invalid base64",
            RecordedCheck {
                script_b64: "@@@@".to_string(),
                timeout_ms: 1_000,
            },
        ),
    ];
    for (name, bad) in bad_checks {
        let mut events = vec![started()];
        set_task(&mut events, Vec::new(), Some(bad));
        assert_rejected_because(name, events, "recorded task is invalid");
    }

    // At the limits: 256 files, a file of exactly the file limit, half the
    // session, and a one-byte script.
    let at_limit = (0..256)
        .map(|index| seed_file(&format!("f{index:04}"), b"x"))
        .collect::<Vec<_>>();
    let mut events = vec![started()];
    set_task(&mut events, at_limit, Some(check("x", half)));
    write_all(&events).expect("256 files and a check of half the session are valid");
    let mut events = vec![started()];
    set_task(
        &mut events,
        vec![seed_file("big", &vec![0; max_file])],
        Some(check("x", 1_000)),
    );
    write_all(&events).expect("a file at the limit is valid");
}

#[test]
fn base64_round_trips_and_refuses_other_encodings() {
    for bytes in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
        let encoded = base64_encode(bytes);
        assert_eq!(base64_decode(&encoded).as_deref(), Some(bytes));
    }
    assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    assert_eq!(base64_encode(b"fo"), "Zm8=");
    let all = (0..=255).collect::<Vec<u8>>();
    assert_eq!(base64_decode(&base64_encode(&all)), Some(all));
    for bad in ["Zm8", "Zm8=Zm8=", "Zg===", "Z===", "Zm9=", "Zm-v", "Zm\nv"] {
        assert_eq!(base64_decode(bad), None, "{bad:?}");
    }
}
