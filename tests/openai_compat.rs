//! The OpenAI-compatible provider against a scripted local HTTP server.
mod common;

use std::time::{Duration, Instant};

use common::http::{FakeHttp, Reply};
use common::{FakeApi, short_lifecycle_config};
use kiln_harness::audit::{JsonlAudit, MemoryAudit};
use kiln_harness::engine::{Harness, HarnessConfig, HarnessError, SessionTask};
use kiln_harness::journal::{JournalEvent, JsonlJournal, MemoryJournal, read_journal};
use kiln_harness::provider::{
    Message, ModelRequest, Provider, ProviderErrorKind, StopReason, ToolCall, ToolDefinition,
    ToolResult, Usage,
};
use kiln_harness::providers::openai_compat::{parse_response, request_body};
use kiln_harness::providers::{OpenAiCompatConfig, OpenAiCompatProvider, SecretString};
use kiln_harness::replay::ReplayProvider;
use kiln_harness::tools::{ToolLimits, ToolRegistry};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const SENTINEL_KEY: &str = "sk-SENTINEL-7c41f0d2e9";

fn config(base_url: &str) -> OpenAiCompatConfig {
    let mut config = OpenAiCompatConfig::new(base_url, "test-model").expect("config");
    config.retry_base_delay = Duration::from_millis(10);
    config.retry_max_delay = Duration::from_millis(50);
    config
}

fn provider(config: OpenAiCompatConfig) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(config).expect("provider")
}

fn usage() -> Value {
    json!({"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120})
}

fn completion(message: &Value, finish_reason: &str) -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "test-model",
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": usage(),
    })
}

fn text_reply(text: &str) -> Reply {
    Reply::json(
        200,
        &completion(&json!({"role": "assistant", "content": text}), "stop"),
    )
}

fn tool_reply(calls: &Value) -> Reply {
    Reply::json(
        200,
        &completion(
            &json!({"role": "assistant", "content": null, "tool_calls": calls}),
            "tool_calls",
        ),
    )
}

fn definitions() -> Vec<ToolDefinition> {
    ToolRegistry::new(ToolLimits::default())
        .definitions()
        .to_vec()
}

fn request<'a>(messages: &'a [Message], tools: &'a [ToolDefinition]) -> ModelRequest<'a> {
    ModelRequest {
        session_id: "hs-test",
        turn: 1,
        system: "Be brief.",
        tools,
        messages,
        max_output_tokens: 256,
    }
}

fn user(text: &str) -> Vec<Message> {
    vec![Message::User {
        text: text.to_string(),
    }]
}

// 1. Request mapping.

#[test]
#[allow(clippy::too_many_lines)] // the golden JSON is long
fn request_body_matches_the_golden_json() {
    let mut config = config("http://127.0.0.1:1/v1");
    config.temperature = 0.7;
    config.seed = Some(42);
    let tools = vec![ToolDefinition {
        name: "list_dir".to_string(),
        description: "List a directory.".to_string(),
        input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    }];
    let messages = vec![
        Message::User {
            text: "Make two files.".to_string(),
        },
        Message::Assistant {
            text: Some("Writing them.".to_string()),
            tool_calls: vec![
                ToolCall::new(
                    "call_a",
                    "write_file",
                    json!({"path": "a.txt", "content": "A\n"}),
                ),
                ToolCall::new("call-1-2", "list_dir", json!({"path": "."})),
            ],
            provider_content: None,
        },
        Message::ToolResults {
            results: vec![
                ToolResult {
                    call_id: "call_a".to_string(),
                    tool: "write_file".to_string(),
                    result: json!({"ok": true, "bytes_written": 2}),
                },
                ToolResult {
                    call_id: "call-1-2".to_string(),
                    tool: "list_dir".to_string(),
                    result: json!({"ok": true, "listing": "a.txt\n"}),
                },
            ],
        },
        Message::Assistant {
            text: None,
            tool_calls: vec![ToolCall::new(
                "x",
                "unknown",
                Value::String("{bad".to_string()),
            )],
            provider_content: None,
        },
    ];
    let request = ModelRequest {
        session_id: "hs-test",
        turn: 3,
        system: "You are careful.",
        tools: &tools,
        messages: &messages,
        max_output_tokens: 512,
    };

    let body = request_body(&config, &request);

    let golden = json!({
        "model": "test-model",
        "messages": [
            {"role": "system", "content": "You are careful."},
            {"role": "user", "content": "Make two files."},
            {
                "role": "assistant",
                "content": "Writing them.",
                "tool_calls": [
                    {
                        "id": "call_a",
                        "type": "function",
                        "function": {
                            "name": "write_file",
                            "arguments": "{\"content\":\"A\\n\",\"path\":\"a.txt\"}"
                        }
                    },
                    {
                        "id": "call-1-2",
                        "type": "function",
                        "function": {"name": "list_dir", "arguments": "{\"path\":\".\"}"}
                    }
                ]
            },
            {
                "role": "tool",
                "tool_call_id": "call_a",
                "content": "{\"bytes_written\":2,\"ok\":true}"
            },
            {
                "role": "tool",
                "tool_call_id": "call-1-2",
                "content": "{\"listing\":\"a.txt\\n\",\"ok\":true}"
            },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "x",
                    "type": "function",
                    "function": {"name": "unknown", "arguments": "\"{bad\""}
                }]
            }
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List a directory.",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
            }
        }],
        "tool_choice": "auto",
        "max_tokens": 512,
        "temperature": 0.7,
        "seed": 42,
        "stream": false
    });
    assert_eq!(body, golden);
    // The decimal as written, not 0.699999988079071.
    assert!(body.to_string().contains("\"temperature\":0.7,"), "{body}");

    let no_system = ModelRequest {
        system: "",
        ..request
    };
    let body = request_body(
        &OpenAiCompatConfig {
            seed: None,
            ..config
        },
        &no_system,
    );
    assert_eq!(
        body["messages"][0]["role"], "user",
        "an empty system is omitted"
    );
    assert!(body.get("seed").is_none());
}

#[tokio::test]
async fn the_request_is_posted_as_json_to_chat_completions() {
    let server = FakeHttp::start([text_reply("hi")]).await;
    let mut provider = provider(config(&server.base_url()));
    let tools = definitions();
    let messages = user("Say hi.");

    let response = provider
        .complete(request(&messages, &tools))
        .await
        .expect("response");

    assert_eq!(response.text.as_deref(), Some("hi"));
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.path, "/v1/chat/completions");
    assert_eq!(sent.header("content-type"), Some("application/json"));
    assert_eq!(sent.header("authorization"), None);
    assert_eq!(
        sent.json(),
        request_body(provider.config(), &request(&messages, &tools))
    );
    assert_eq!(sent.json()["tools"].as_array().map(Vec::len), Some(5));
}

// 2. Response mapping.

#[test]
fn finish_reasons_map_to_stop_reasons() {
    let call = json!([{"id": "c1", "type": "function",
        "function": {"name": "list_dir", "arguments": "{\"path\":\".\"}"}}]);
    let text = json!({"role": "assistant", "content": "done"});
    let with_call = json!({"role": "assistant", "content": "", "tool_calls": call});
    let cases = [
        (&text, "stop", StopReason::EndTurn),
        (&with_call, "tool_calls", StopReason::ToolUse),
        (&with_call, "function_call", StopReason::ToolUse),
        // Ollama reports tool calls with finish_reason "stop".
        (&with_call, "stop", StopReason::ToolUse),
        (&text, "length", StopReason::MaxTokens),
        (&text, "content_filter", StopReason::Refusal),
        (&text, "eos_token", StopReason::Other),
    ];
    for (message, finish, expected) in cases {
        let response = parse_response(&completion(message, finish), 1).expect("parses");
        assert_eq!(response.stop_reason, expected, "{finish}");
    }
    let mut missing = completion(&text, "stop");
    missing["choices"][0]
        .as_object_mut()
        .expect("choice")
        .remove("finish_reason");
    assert_eq!(
        parse_response(&missing, 1).expect("parses").stop_reason,
        StopReason::Other
    );

    let response = parse_response(&completion(&text, "stop"), 1).expect("parses");
    assert_eq!(response.text.as_deref(), Some("done"));
    assert!(response.tool_calls.is_empty());
    let response = parse_response(&completion(&with_call, "stop"), 1).expect("parses");
    assert_eq!(response.text, None, "empty content is no text");
    assert_eq!(response.tool_calls.len(), 1);
}

#[test]
fn reasoning_side_channels_and_unknown_fields_are_ignored() {
    let message = json!({
        "role": "assistant",
        "content": "answer",
        "reasoning_content": "secret chain of thought",
        "reasoning": "more",
        "refusal": null,
        "annotations": [],
    });
    let mut body = completion(&message, "stop");
    body["system_fingerprint"] = json!("fp");
    body["choices"][0]["logprobs"] = json!(null);
    let response = parse_response(&body, 1).expect("parses");
    assert_eq!(response.text.as_deref(), Some("answer"));
    let encoded = serde_json::to_string(&response).expect("encode");
    assert!(!encoded.contains("chain of thought"));
}

#[test]
fn malformed_responses_are_invalid() {
    let message = json!({"role": "assistant", "content": "x"});
    let mut cases = vec![
        json!({}),
        json!({"choices": [], "usage": usage()}),
        json!({"choices": [{"finish_reason": "stop"}], "usage": usage()}),
        json!({"error": {"message": "model not loaded"}}),
        completion(&json!({"role": "assistant", "content": 7}), "stop"),
        completion(
            &json!({"role": "assistant", "tool_calls": [{"id": "a"}]}),
            "tool_calls",
        ),
        completion(
            &json!({"role": "assistant", "tool_calls": [{"function": {"arguments": "{}"}}]}),
            "tool_calls",
        ),
    ];
    let mut no_completion_tokens = completion(&message, "stop");
    no_completion_tokens["usage"] = json!({"prompt_tokens": 3});
    cases.push(no_completion_tokens);
    for body in cases {
        let error = parse_response(&body, 1).expect_err(&body.to_string());
        assert_eq!(error.kind, ProviderErrorKind::InvalidResponse, "{body}");
        assert!(!error.retryable);
    }
    let error = parse_response(&json!({"error": {"message": "model not loaded"}}), 1)
        .expect_err("error body");
    assert!(error.message.contains("model not loaded"), "{error}");
}

// 3. Arguments and call IDs.

#[test]
fn arguments_are_parsed_or_kept_for_the_tool_to_reject() {
    let calls = json!([
        {"id": "call_1", "type": "function",
         "function": {"name": "write_file", "arguments": "{\"path\":\"a\",\"content\":\"b\"}"}},
        {"id": "call_2", "type": "function",
         "function": {"name": "list_dir", "arguments": {"path": "."}}},
        {"id": "call_3", "type": "function",
         "function": {"name": "list_dir", "arguments": "{\"path\": \".\""}},
        {"id": "call_4", "type": "function",
         "function": {"name": "list_dir", "arguments": "[\".\"]"}},
        {"type": "function", "function": {"name": "workspace_diff", "arguments": "{}"}},
        {"id": "bad id with spaces", "function": {"name": "workspace_diff", "arguments": "{}"}},
        {"id": "x".repeat(65), "function": {"name": "workspace_diff", "arguments": "{}"}},
        {"id": 17, "function": {"name": "workspace_diff", "arguments": "{}"}},
    ]);
    let response = parse_response(
        &completion(
            &json!({"role": "assistant", "content": null, "tool_calls": calls}),
            "tool_calls",
        ),
        4,
    )
    .expect("parses");
    let calls = response.tool_calls;
    assert_eq!(calls[0].arguments, json!({"path": "a", "content": "b"}));
    assert_eq!(calls[1].arguments, json!({"path": "."}));
    assert_eq!(calls[2].arguments, json!("{\"path\": \".\""));
    assert_eq!(calls[3].arguments, json!("[\".\"]"));
    assert_eq!(calls[4].arguments, json!({}));
    let ids = calls
        .iter()
        .map(|call| call.call_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        [
            "call_1", "call_2", "call_3", "call_4", "call-4-5", "call-4-6", "call-4-7", "call-4-8"
        ]
    );
    assert_eq!(calls[0].tool, "write_file");
}

#[tokio::test]
async fn invalid_arguments_reach_the_model_as_a_tool_error() {
    let server = FakeHttp::start([
        tool_reply(&json!([{"id": "call_w", "type": "function",
            "function": {"name": "write_file", "arguments": "{\"path\": \"a.txt\""}}])),
        text_reply("I could not write the file."),
    ])
    .await;
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        provider(config(&server.base_url())),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );

    let outcome = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("session completes");

    assert_eq!(outcome.final_answer, "I could not write the file.");
    let second = &server.requests()[1].json();
    let messages = second["messages"].as_array().expect("messages");
    let tool = messages.last().expect("tool message");
    assert_eq!(tool["role"], "tool");
    assert_eq!(tool["tool_call_id"], "call_w");
    let result: Value =
        serde_json::from_str(tool["content"].as_str().expect("content")).expect("result JSON");
    assert_eq!(result["ok"], false);
    assert_eq!(result["error"], "invalid_arguments");
    // The raw arguments go back as the JSON string the model wrote.
    let assistant = &messages[messages.len() - 2];
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"],
        json!("\"{\\\"path\\\": \\\"a.txt\\\"\"")
    );
    assert!(
        api.state()
            .execs
            .iter()
            .all(|exec| exec.argv[0] != "/bin/dd")
    );
}

// 4. Usage.

#[test]
fn usage_is_normalized_to_disjoint_counters() {
    let mut body = completion(&json!({"role": "assistant", "content": "x"}), "stop");
    body["usage"] = json!({
        "prompt_tokens": 1000,
        "completion_tokens": 50,
        "total_tokens": 1050,
        "prompt_tokens_details": {"cached_tokens": 600},
        "completion_tokens_details": {"reasoning_tokens": 30},
    });
    assert_eq!(
        parse_response(&body, 1).expect("parses").usage,
        Usage {
            input_tokens: 400,
            output_tokens: 50,
            cache_read_input_tokens: 600,
            cache_creation_input_tokens: 0,
        }
    );
    body["usage"]["prompt_tokens_details"] = json!(null);
    assert_eq!(
        parse_response(&body, 1).expect("parses").usage.input_tokens,
        1000
    );
    body["usage"]["prompt_tokens_details"] = json!({"cached_tokens": 5000});
    let usage = parse_response(&body, 1).expect("parses").usage;
    assert_eq!(usage.input_tokens, 0, "saturating");
    assert_eq!(usage.cache_read_input_tokens, 5000);
}

#[tokio::test]
async fn a_response_without_usage_fails_closed_without_retrying() {
    let mut body = completion(&json!({"role": "assistant", "content": "x"}), "stop");
    body.as_object_mut().expect("object").remove("usage");
    let server = FakeHttp::start([Reply::json(200, &body)]).await;
    let mut provider = provider(config(&server.base_url()));
    let tools = definitions();
    let messages = user("hi");

    let error = provider
        .complete(request(&messages, &tools))
        .await
        .expect_err("no usage");

    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
    assert!(error.message.contains("usage"), "{error}");
    assert!(!error.retryable);
    assert_eq!(server.requests().len(), 1);
}

// 5. Retries.

async fn complete_once(
    config: OpenAiCompatConfig,
) -> Result<kiln_harness::provider::ModelResponse, kiln_harness::provider::ProviderError> {
    let mut provider = provider(config);
    let tools = definitions();
    let messages = user("hi");
    provider.complete(request(&messages, &tools)).await
}

#[tokio::test]
async fn retry_after_is_honored() {
    let server = FakeHttp::start([
        Reply::status(429, "slow down").with_header("Retry-After", "1"),
        text_reply("ok"),
    ])
    .await;
    let mut config = config(&server.base_url());
    config.retry_max_delay = Duration::from_secs(5);

    let response = complete_once(config).await.expect("second attempt");

    assert_eq!(response.text.as_deref(), Some("ok"));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let gap = requests[1].at - requests[0].at;
    assert!(gap >= Duration::from_millis(950), "{gap:?}");
    assert!(gap < Duration::from_secs(3), "{gap:?}");
}

#[tokio::test]
async fn retry_after_is_capped_at_the_max_delay() {
    let server = FakeHttp::start([
        Reply::status(429, "").with_header("Retry-After", "3600"),
        Reply::status(503, "").with_header("Retry-After", "Fri, 31 Dec 9999 23:59:59 GMT"),
        text_reply("ok"),
    ])
    .await;
    let mut config = config(&server.base_url());
    config.retry_max_delay = Duration::from_millis(200);
    let started = Instant::now();

    let response = complete_once(config).await.expect("third attempt");

    assert_eq!(response.text.as_deref(), Some("ok"));
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    for pair in requests.windows(2) {
        let gap = pair[1].at - pair[0].at;
        assert!(gap >= Duration::from_millis(190), "{gap:?}");
        assert!(gap < Duration::from_secs(2), "{gap:?}");
    }
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn overload_is_retried_until_attempts_run_out() {
    let server = FakeHttp::start([
        Reply::status(503, "busy 1"),
        Reply::status(503, "busy 2"),
        Reply::status(503, "busy 3"),
        text_reply("never reached"),
    ])
    .await;

    let error = complete_once(config(&server.base_url()))
        .await
        .expect_err("attempts exhausted");

    assert_eq!(error.kind, ProviderErrorKind::Overloaded);
    assert!(error.retryable);
    assert_eq!(error.message, "HTTP 503: busy 3", "the last error");
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn client_errors_are_not_retried() {
    let server = FakeHttp::start([Reply::status(401, "{\"error\":\"bad key\"}")]).await;
    let error = complete_once(config(&server.base_url()))
        .await
        .expect_err("rejected");
    assert_eq!(error.kind, ProviderErrorKind::Rejected);
    assert!(!error.retryable);
    assert_eq!(error.message, "HTTP 401: {\"error\":\"bad key\"}");
    assert_eq!(server.requests().len(), 1);

    let server = FakeHttp::start([Reply::json(
        400,
        &json!({"error": {"message": "This model's maximum context length is 4096 tokens. \
            However, you requested 9000 tokens.", "type": "invalid_request_error"}}),
    )])
    .await;
    let error = complete_once(config(&server.base_url()))
        .await
        .expect_err("overflow");
    assert_eq!(error.kind, ProviderErrorKind::ContextOverflow);
    assert!(!error.retryable);
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn dropped_connections_are_retried() {
    let server = FakeHttp::start([Reply::Close, Reply::Close, text_reply("third time")]).await;
    let response = complete_once(config(&server.base_url()))
        .await
        .expect("retried");
    assert_eq!(response.text.as_deref(), Some("third time"));
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn connection_refused_is_retried_then_a_transport_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("address");
    drop(listener);
    let mut config = config(&format!("http://{addr}/v1"));
    config.retry_base_delay = Duration::from_millis(100);
    config.retry_max_delay = Duration::from_millis(100);
    let mut provider = provider(config);
    let tools = definitions();
    let messages = user("hi");
    // Jitter is seeded from the session and turn, so the schedule is fixed.
    let started = Instant::now();

    let error = provider
        .complete(request(&messages, &tools))
        .await
        .expect_err("refused");

    assert_eq!(error.kind, ProviderErrorKind::Transport);
    assert!(error.retryable);
    assert!(error.message.starts_with("connect failed"), "{error}");
    assert!(!error.message.contains(&addr.to_string()), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn a_server_that_comes_up_during_backoff_is_reached() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("address");
    drop(listener);
    let mut config = config(&format!("http://{addr}/v1"));
    config.max_attempts = 8;
    config.retry_base_delay = Duration::from_millis(200);
    config.retry_max_delay = Duration::from_millis(400);
    let mut provider = provider(config);
    let tools = definitions();
    let messages = user("hi");
    let late = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        FakeHttp::start_at(addr, [text_reply("up now")]).await
    });

    let response = provider
        .complete(request(&messages, &tools))
        .await
        .expect("a later attempt connects");

    assert_eq!(response.text.as_deref(), Some("up now"));
    assert_eq!(late.await.expect("server").requests().len(), 1);
}

// 6. and 7. Body bounds.

#[tokio::test]
async fn an_endless_response_is_cut_off_at_the_cap() {
    let server = FakeHttp::start([Reply::StreamForever]).await;
    let mut config = config(&server.base_url());
    config.max_response_bytes = 64 << 10;

    let error = complete_once(config).await.expect_err("too large");

    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
    assert_eq!(
        error.message,
        format!("response exceeds {} bytes", 64 << 10)
    );
    assert!(!error.retryable);
    // The client hung up: the server's writes fail. Socket buffers let it
    // write somewhat past the cap before that, but not without bound.
    server
        .wait_for(Duration::from_secs(5), |state| state.streams_ended == 1)
        .await;
    assert!(server.state().streamed_bytes < 32 << 20);
    assert_eq!(server.requests().len(), 1, "never retried");
}

#[tokio::test]
async fn a_declared_length_over_the_cap_is_refused() {
    let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(200 << 10));
    let server = FakeHttp::start([Reply::Respond {
        status: 200,
        headers: vec![],
        body: big.into_bytes(),
        delay: Duration::ZERO,
    }])
    .await;
    let mut config = config(&server.base_url());
    config.max_response_bytes = 100 << 10;
    let error = complete_once(config).await.expect_err("too large");
    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
    assert!(error.message.contains("exceeds"), "{error}");
}

#[tokio::test]
async fn an_oversized_request_is_never_sent() {
    let server = FakeHttp::start([text_reply("unused")]).await;
    let mut config = config(&server.base_url());
    config.max_request_bytes = 1024;
    let mut provider = provider(config);
    let tools = definitions();
    let messages = user(&"x".repeat(2048));

    let error = provider
        .complete(request(&messages, &tools))
        .await
        .expect_err("too large");

    assert_eq!(error.kind, ProviderErrorKind::ContextOverflow);
    assert!(!error.retryable);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(server.state().connections, 0);
}

// 8. A full session, then its replay without the server.

fn task() -> SessionTask {
    SessionTask::new(
        "Use the tools.".to_string(),
        "Write hello to a.txt, read it back, then report.".to_string(),
    )
}

fn session_script() -> Vec<Reply> {
    vec![
        tool_reply(
            &json!([{"id": "call_write", "type": "function", "function": {
            "name": "write_file",
            "arguments": "{\"path\":\"a.txt\",\"content\":\"hello\\n\"}"}}]),
        ),
        // Two calls in one turn; the second has no id and gets call-2-2.
        Reply::json(
            200,
            &completion(
                &json!({"role": "assistant", "content": "Checking.", "tool_calls": [
                    {"id": "call_read", "type": "function", "function": {
                        "name": "read_file", "arguments": {"path": "a.txt"}}},
                    {"type": "function", "function": {
                        "name": "list_dir", "arguments": "{\"path\":\".\"}"}}
                ]}),
                // Ollama style.
                "stop",
            ),
        ),
        text_reply("a.txt contains hello."),
    ]
}

#[tokio::test]
async fn a_recorded_session_replays_without_the_server() {
    let temp = common::private_tempdir().expect("tempdir");
    let recorded = temp.path().join("run.jsonl");
    let server = FakeHttp::start(session_script()).await;
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        provider(config(&server.base_url())),
        MemoryAudit::default(),
        JsonlJournal::open(&recorded).expect("journal"),
        short_lifecycle_config(),
    );

    let outcome = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("session completes");

    assert_eq!(outcome.final_answer, "a.txt contains hello.");
    assert_eq!((outcome.steps, outcome.tool_calls), (3, 3));
    assert_eq!(
        outcome.usage,
        Usage {
            input_tokens: 300,
            output_tokens: 60,
            ..Usage::default()
        }
    );
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let third = requests[2].json();
    let messages = third["messages"].as_array().expect("messages");
    let routed = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["tool_call_id"].as_str().expect("id"))
        .collect::<Vec<_>>();
    assert_eq!(routed, ["call_write", "call_read", "call-2-2"]);
    let read: Value = serde_json::from_str(
        messages[messages.len() - 2]["content"]
            .as_str()
            .expect("content"),
    )
    .expect("JSON");
    assert_eq!(read["content"], "hello\n");

    let journal = read_journal(&recorded).expect("journal verifies");
    assert!(matches!(
        &journal.records[0].event,
        JournalEvent::SessionStarted { provider, .. }
            if provider.name == "openai-compat" && provider.model == "test-model"
    ));

    // The model server is gone; replay needs only the journal and kilnd.
    server.stop();
    let replay = ReplayProvider::from_journal(&journal).expect("replayable");
    let mut config = short_lifecycle_config();
    config.apply_limits(&replay.limits());
    let replay_task = replay.task();
    let fresh = FakeApi::default();
    let mut harness = Harness::new(
        fresh.clone(),
        replay,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let replayed = harness
        .run(&replay_task, CancellationToken::new())
        .await
        .expect("replay completes");
    assert_eq!(replayed.final_answer, outcome.final_answer);
    assert_eq!(harness.provider().diverged_at(), None);
    assert!(harness.provider().exhausted(), "reproduced");
    assert_eq!(fresh.state().deleted, ["sb-0000000000000001"]);
}

// 9. Deadlines and cancellation.

#[tokio::test]
async fn a_hanging_server_hits_the_session_deadline_and_the_request_is_dropped() {
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("hang.jsonl");
    let server = FakeHttp::start([Reply::Hang]).await;
    let api = FakeApi::default();
    let mut config = short_lifecycle_config();
    config.session_timeout = Duration::from_millis(500);
    let mut harness = Harness::new(
        api.clone(),
        provider(self::config(&server.base_url())),
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        config,
    );
    let started = Instant::now();

    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("deadline");

    assert!(matches!(error, HarnessError::Deadline), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
    server
        .wait_for(Duration::from_secs(2), |state| state.client_closed == 1)
        .await;
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
    let journal = read_journal(&path).expect("journal verifies");
    assert!(matches!(
        journal.records.last().map(|record| &record.event),
        Some(JournalEvent::SessionEnded { work_outcome, sandbox_deleted: true, .. })
            if work_outcome == "deadline"
    ));
}

#[tokio::test]
async fn cancel_during_backoff_is_prompt() {
    let server = FakeHttp::start([
        Reply::status(503, "busy").with_header("Retry-After", "30"),
        text_reply("never"),
    ])
    .await;
    let mut provider_config = config(&server.base_url());
    provider_config.retry_max_delay = Duration::from_secs(60);
    let api = FakeApi::default();
    let mut config = short_lifecycle_config();
    config.session_timeout = Duration::from_secs(120);
    let mut harness = Harness::new(
        api.clone(),
        provider(provider_config),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let wait_server = server.state.clone();
    tokio::spawn(async move {
        // Cancel once the first attempt has been answered and backoff began.
        loop {
            if !wait_server.lock().expect("state").requests.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let started = Instant::now();

    let error = harness.run(&task(), cancel).await.expect_err("canceled");

    assert!(matches!(error, HarnessError::Canceled), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(server.requests().len(), 1, "no attempt after the cancel");
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
}

// 10. The API key stays out of every record.

#[tokio::test]
async fn the_api_key_reaches_only_the_authorization_header() {
    let temp = common::private_tempdir().expect("tempdir");
    let audit_path = temp.path().join("audit.jsonl");
    let journal_path = temp.path().join("run.jsonl");
    let failed_path = temp.path().join("failed.jsonl");
    let server = FakeHttp::start(session_script().into_iter().chain([Reply::status(
        401,
        &format!("{{\"error\":\"invalid key {SENTINEL_KEY}\"}}"),
    )]))
    .await;
    let mut keyed = config(&server.base_url());
    keyed.api_key = Some(SecretString::new(SENTINEL_KEY));
    assert!(!format!("{keyed:?}").contains(SENTINEL_KEY));
    let provider = provider(keyed.clone());
    assert!(!format!("{provider:?}").contains(SENTINEL_KEY));

    let mut harness = Harness::new(
        FakeApi::default(),
        provider,
        JsonlAudit::open(&audit_path).expect("audit"),
        JsonlJournal::open(&journal_path).expect("journal"),
        short_lifecycle_config(),
    );
    harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("session completes");

    // A server that echoes the key in an error body.
    let mut harness = Harness::new(
        FakeApi::default(),
        self::provider(keyed),
        JsonlAudit::open(&audit_path).expect("audit"),
        JsonlJournal::open(&failed_path).expect("journal"),
        short_lifecycle_config(),
    );
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("rejected");
    let message = error.to_string();
    assert!(
        message.contains("HTTP 401 (body withheld: authenticated request)"),
        "{message}"
    );
    assert!(!format!("{error:?}").contains(SENTINEL_KEY));

    for path in [&audit_path, &journal_path, &failed_path] {
        let text = std::fs::read_to_string(path).expect("read");
        assert!(!text.is_empty());
        assert!(
            !text.contains(SENTINEL_KEY),
            "{} leaks the key",
            path.display()
        );
        assert!(
            !text.contains("SENTINEL"),
            "{} leaks part of the key",
            path.display()
        );
    }
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests {
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {SENTINEL_KEY}").as_str())
        );
        assert!(!String::from_utf8_lossy(&request.body).contains(SENTINEL_KEY));
    }
}

#[test]
fn a_key_over_plain_http_to_a_remote_host_is_refused() {
    let mut config = config("http://192.168.1.20:11434/v1");
    config.api_key = Some(SecretString::new(SENTINEL_KEY));
    let error = OpenAiCompatProvider::new(config).expect_err("https required");
    assert!(error.to_string().contains("https"), "{error}");
    assert!(!error.to_string().contains(SENTINEL_KEY));
}

#[test]
fn config_validation_refuses_bad_values() {
    for url in [
        "ftp://h/v1",
        "file:///tmp/sock",
        "http://user:pw@h/v1",
        "http://user@h/v1",
        "http://h/v1?key=1",
        "http://h/v1#frag",
    ] {
        let config = OpenAiCompatConfig::new(url, "m").expect("parses");
        assert!(config.validate().is_err(), "{url}");
    }
    assert!(OpenAiCompatConfig::new("not a url", "m").is_err());
    let good = config("http://127.0.0.1:1/v1");
    good.validate().expect("defaults are valid");
    let bad = [
        OpenAiCompatConfig {
            model: String::new(),
            ..good.clone()
        },
        OpenAiCompatConfig {
            model: "m\n".to_string(),
            ..good.clone()
        },
        OpenAiCompatConfig {
            model: "m".repeat(257),
            ..good.clone()
        },
        OpenAiCompatConfig {
            temperature: 2.5,
            ..good.clone()
        },
        OpenAiCompatConfig {
            temperature: f32::NAN,
            ..good.clone()
        },
        OpenAiCompatConfig {
            max_response_bytes: (64 << 20) + 1,
            ..good.clone()
        },
        OpenAiCompatConfig {
            max_request_bytes: 0,
            ..good.clone()
        },
        OpenAiCompatConfig {
            max_attempts: 9,
            ..good.clone()
        },
        OpenAiCompatConfig {
            max_attempts: 0,
            ..good.clone()
        },
        OpenAiCompatConfig {
            retry_max_delay: Duration::from_millis(1),
            ..good.clone()
        },
        OpenAiCompatConfig {
            connect_timeout: Duration::ZERO,
            ..good.clone()
        },
    ];
    for config in bad {
        assert!(config.validate().is_err(), "{config:?}");
    }
}

#[test]
fn harness_defaults_for_run_pass_validation() {
    let config = HarnessConfig {
        max_steps: 40,
        max_tool_calls: 120,
        session_timeout: Duration::from_secs(900),
        tool_limits: ToolLimits {
            timeout: Duration::from_secs(30),
            ..ToolLimits::default()
        },
        ..HarnessConfig::default()
    };
    config.validate().expect("run defaults");
}

#[tokio::test]
async fn tool_calls_finish_with_zero_calls_fails_closed() {
    // finish_reason "tool_calls" with no calls maps to ToolUse, which the
    // engine refuses as a protocol fault rather than a final answer.
    let server = FakeHttp::start([Reply::json(
        200,
        &completion(
            &json!({"role": "assistant", "content": "done?", "tool_calls": []}),
            "tool_calls",
        ),
    )])
    .await;
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        provider(config(&server.base_url())),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("fails closed");
    assert!(
        matches!(&error, HarnessError::InvalidModelCall(reason) if reason == "tool_use stop with no tool calls"),
        "{error}"
    );
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
}
