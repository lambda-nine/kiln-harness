//! The Anthropic Messages API provider against a scripted local HTTP server.
mod common;

use std::time::Duration;

use common::http::{FakeHttp, Reply};
use common::{FakeApi, short_lifecycle_config};
use kiln_harness::audit::{AuditEvent, JsonlAudit, MemoryAudit};
use kiln_harness::engine::{Harness, HarnessError, SessionTask};
use kiln_harness::journal::{JournalEvent, JsonlJournal, MemoryJournal, read_journal};
use kiln_harness::provider::{
    Message, ModelRequest, ModelResponse, Provider, ProviderContent, ProviderError,
    ProviderErrorKind, StopReason, ToolCall, ToolDefinition, ToolResult, Usage,
};
use kiln_harness::providers::anthropic::{
    API_VERSION, FALLBACK_BETA, parse_response, request_body,
};
use kiln_harness::providers::{AnthropicConfig, AnthropicProvider, Effort, SecretString};
use kiln_harness::replay::ReplayProvider;
use kiln_harness::tools::{ToolLimits, ToolRegistry};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const SENTINEL_KEY: &str = "sk-ant-SENTINEL-4b19e2c7";
const CREDENTIAL_ECHO: &str = "response contained the request credential";
const SIGNATURE: &str = "EqQBCkYIBxgCKkDsig/n+ature==";

fn base(server: &FakeHttp) -> String {
    format!("http://{}", server.addr)
}

fn config(base_url: &str) -> AnthropicConfig {
    let mut config = AnthropicConfig::new(
        base_url,
        "claude-opus-5-5",
        Some(SecretString::new(SENTINEL_KEY)),
    )
    .expect("config");
    config.retry_base_delay = Duration::from_millis(5);
    config.retry_max_delay = Duration::from_millis(40);
    config
}

fn provider(config: AnthropicConfig) -> AnthropicProvider {
    AnthropicProvider::new(config).expect("provider")
}

/// The live non-fallback usage shape; see `common::anthropic`.
fn usage(input: u64, output: u64) -> Value {
    common::anthropic::live_usage(input, output, 0, 0)
}

fn message(content: &Value, stop_reason: &str, usage: &Value) -> Value {
    json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": content,
        "stop_reason": stop_reason,
        "stop_details": null,
        "usage": usage,
    })
}

fn ok(content: &Value, stop_reason: &str) -> Reply {
    Reply::json(200, &message(content, stop_reason, &usage(100, 40)))
}

fn thinking(text: &str) -> Value {
    json!({"type": "thinking", "thinking": text, "signature": SIGNATURE})
}

fn tool_use(id: &str, name: &str, input: &Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

fn text(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn anthropic_content(blocks: Value) -> ProviderContent {
    ProviderContent {
        provider: "anthropic".to_string(),
        model: Some("claude-opus-5-5".to_string()),
        blocks,
        stop_details: None,
    }
}

fn definitions() -> Vec<ToolDefinition> {
    ToolRegistry::new(ToolLimits::default())
        .definitions()
        .to_vec()
}

fn error_body(kind: &str, message: &str) -> String {
    json!({"type": "error", "error": {"type": kind, "message": message},
           "request_id": "req_011CTest42"})
    .to_string()
}

async fn complete_once(server: &FakeHttp) -> Result<ModelResponse, ProviderError> {
    complete_with(config(&base(server))).await
}

async fn complete_with(config: AnthropicConfig) -> Result<ModelResponse, ProviderError> {
    let mut provider = provider(config);
    let messages = [Message::User {
        text: "hi".to_string(),
    }];
    provider
        .complete(ModelRequest {
            session_id: "hs-anthropic",
            turn: 1,
            system: "",
            tools: &[],
            messages: &messages,
            max_output_tokens: 64,
        })
        .await
}

// 1. Request mapping.

#[test]
#[allow(clippy::too_many_lines)] // the golden JSON is long
fn request_body_matches_the_golden_json() {
    let mut config = config("https://api.anthropic.com");
    config.effort = Effort::Xhigh;
    let tools = vec![ToolDefinition {
        name: "list_dir".to_string(),
        description: "List a directory.".to_string(),
        input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    }];
    // An earlier turn exactly as the API returned it, fallback included.
    let blocks = json!([
        {"type": "fallback", "from": {"model": "claude-opus-5-5"}, "to": {"model": "claude-opus-4-8"}},
        thinking(""),
        text("Writing both."),
        tool_use("toolu_01A", "write_file", &json!({"path": "a.txt", "content": "A\n"})),
        tool_use("toolu_01B", "list_dir", &json!({"path": "."})),
    ]);
    let messages = vec![
        Message::User {
            text: "Make a file.".to_string(),
        },
        Message::Assistant {
            // The neutral view differs on purpose: the echo must ignore it.
            text: Some("sanitized view".to_string()),
            tool_calls: vec![
                ToolCall::new("toolu_01A", "write_file", json!({"path": "a.txt"})),
                ToolCall::new("toolu_01B", "list_dir", json!({"path": "."})),
            ],
            provider_content: Some(anthropic_content(blocks.clone())),
        },
        Message::ToolResults {
            results: vec![
                ToolResult {
                    call_id: "toolu_01A".to_string(),
                    tool: "write_file".to_string(),
                    result: json!({"ok": true, "bytes_written": 2}),
                },
                ToolResult {
                    call_id: "toolu_01B".to_string(),
                    tool: "list_dir".to_string(),
                    result: json!({"ok": false, "error": "invalid_path"}),
                },
            ],
        },
    ];
    let request = ModelRequest {
        session_id: "hs-test",
        turn: 2,
        system: "You are careful.",
        tools: &tools,
        messages: &messages,
        max_output_tokens: 16_384,
    };

    let body = request_body(&config, &request).expect("body");

    let golden = json!({
        "model": "claude-opus-5-5",
        "max_tokens": 16_384,
        "system": "You are careful.",
        "messages": [
            {"role": "user", "content": "Make a file."},
            {"role": "assistant", "content": blocks},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01A",
                 "content": "{\"bytes_written\":2,\"ok\":true}", "is_error": false},
                {"type": "tool_result", "tool_use_id": "toolu_01B",
                 "content": "{\"error\":\"invalid_path\",\"ok\":false}", "is_error": true}
            ]}
        ],
        "tools": [{
            "name": "list_dir",
            "description": "List a directory.",
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
        }],
        "tool_choice": {"type": "auto"},
        "output_config": {"effort": "xhigh"},
        "cache_control": {"type": "ephemeral"}
    });
    assert_eq!(body, golden);
    // The echoed turn is byte-identical to the recorded blocks.
    assert_eq!(
        serde_json::to_string(&body["messages"][1]["content"]).expect("encode"),
        serde_json::to_string(&blocks).expect("encode")
    );
    // Fallbacks are off by default, so the request carries none.
    for absent in [
        "temperature",
        "top_p",
        "top_k",
        "thinking",
        "stream",
        "fallbacks",
    ] {
        assert!(body.get(absent).is_none(), "{absent} must not be sent");
    }
    let mut opted_in = config.clone();
    opted_in.fallbacks = true;
    assert_eq!(
        request_body(&opted_in, &request).expect("body")["fallbacks"],
        "default"
    );
}

#[test]
fn optional_fields_follow_the_configuration() {
    let messages = vec![Message::User {
        text: "hi".to_string(),
    }];
    let tools = definitions();
    let request = |tools| ModelRequest {
        session_id: "hs-test",
        turn: 1,
        system: "",
        tools,
        messages: &messages,
        max_output_tokens: 8,
    };
    let mut config = config("https://api.anthropic.com");
    config.fallbacks = false;
    config.strict_tools = true;
    let bare = request_body(&config, &request(&[])).expect("body");
    for absent in ["system", "tools", "tool_choice", "fallbacks"] {
        assert!(bare.get(absent).is_none(), "{absent}: {bare}");
    }
    assert_eq!(bare["output_config"], json!({"effort": "high"}));
    let strict = request_body(&config, &request(&tools)).expect("body");
    let schemas = strict["tools"].as_array().expect("tools");
    assert_eq!(schemas.len(), 5);
    assert!(schemas.iter().all(|tool| tool["strict"] == true));
    config.strict_tools = false;
    let loose = request_body(&config, &request(&tools)).expect("body");
    assert!(
        loose["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .all(|tool| tool.get("strict").is_none())
    );
}

#[test]
fn a_transcript_from_another_provider_is_rebuilt_as_blocks() {
    let config = config("https://api.anthropic.com");
    let messages = vec![
        Message::User {
            text: "go".to_string(),
        },
        Message::Assistant {
            text: Some("Listing.".to_string()),
            tool_calls: vec![
                ToolCall::new("call_a", "list_dir", json!({"path": "."})),
                ToolCall::new("call-1-2", "exec", Value::String("{bad".to_string())),
            ],
            provider_content: Some(ProviderContent {
                provider: "openai-compat".to_string(),
                model: None,
                blocks: json!([{"type": "not-ours"}]),
                stop_details: None,
            }),
        },
        Message::Assistant {
            text: None,
            tool_calls: vec![ToolCall::new("call_b", "list_dir", json!({}))],
            provider_content: None,
        },
    ];
    let body = request_body(
        &config,
        &ModelRequest {
            session_id: "hs-test",
            turn: 3,
            system: "",
            tools: &[],
            messages: &messages,
            max_output_tokens: 8,
        },
    )
    .expect("body");
    assert_eq!(
        body["messages"][1],
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "Listing."},
            {"type": "tool_use", "id": "call_a", "name": "list_dir", "input": {"path": "."}},
            {"type": "tool_use", "id": "call-1-2", "name": "exec", "input": {}}
        ]})
    );
    assert_eq!(
        body["messages"][2],
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "call_b", "name": "list_dir", "input": {}}
        ]})
    );
}

#[tokio::test]
async fn the_request_carries_the_api_headers_and_the_key_only_in_x_api_key() {
    let server = FakeHttp::start([ok(&json!([text("hi")]), "end_turn")]).await;
    complete_once(&server).await.expect("response");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/messages");
    assert_eq!(request.header("x-api-key"), Some(SENTINEL_KEY));
    assert_eq!(request.header("anthropic-version"), Some(API_VERSION));
    // Fallbacks are opt-in: no beta header and no field by default.
    assert_eq!(request.header("anthropic-beta"), None);
    assert!(request.json().get("fallbacks").is_none());
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("authorization"), None);
    assert!(!String::from_utf8_lossy(&request.body).contains(SENTINEL_KEY));
    for absent in ["temperature", "top_p", "top_k", "thinking", "stream"] {
        assert!(request.json().get(absent).is_none(), "{absent}");
    }

    let server = FakeHttp::start([ok(&json!([text("hi")]), "end_turn")]).await;
    let mut fallbacks = config(&base(&server));
    fallbacks.fallbacks = true;
    complete_with(fallbacks).await.expect("response");
    let request = &server.requests()[0];
    assert_eq!(request.header("anthropic-beta"), Some(FALLBACK_BETA));
    assert_eq!(request.json()["fallbacks"], "default");
}

#[tokio::test]
async fn an_oversized_request_is_never_sent() {
    let server = FakeHttp::start([ok(&json!([text("never")]), "end_turn")]).await;
    let mut small = config(&base(&server));
    small.max_request_bytes = 64;
    let error = complete_with(small).await.expect_err("too large");
    assert_eq!(error.kind, ProviderErrorKind::ContextOverflow);
    assert_eq!(server.state().connections, 0);
}

// 2. Response mapping.

fn parsed(content: &Value, stop_reason: &str) -> ModelResponse {
    parse_response(
        &message(content, stop_reason, &usage(100, 400)),
        "claude-opus-5-5",
        true,
    )
    .expect("valid response")
}

#[test]
fn text_blocks_join_into_the_neutral_text() {
    let response = parsed(&json!([text("one"), text("two")]), "end_turn");
    assert_eq!(response.text.as_deref(), Some("one\ntwo"));
    assert!(response.tool_calls.is_empty());
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    let content = response.provider_content.expect("content");
    assert_eq!(content.provider, "anthropic");
    assert_eq!(content.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(content.blocks, json!([text("one"), text("two")]));
    assert_eq!(content.stop_details, None);

    let empty = parsed(&json!([text("")]), "end_turn");
    assert_eq!(empty.text, None);
}

#[test]
fn thinking_and_two_tool_calls_keep_their_blocks() {
    let blocks = json!([
        thinking(""),
        tool_use(
            "toolu_1",
            "write_file",
            &json!({"path": "a", "content": "x"})
        ),
        tool_use("toolu_2", "read_file", &json!({"path": "a"})),
    ]);
    let response = parsed(&blocks, "tool_use");
    assert_eq!(response.text, None);
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(
        response.tool_calls,
        [
            ToolCall::new(
                "toolu_1",
                "write_file",
                json!({"path": "a", "content": "x"})
            ),
            ToolCall::new("toolu_2", "read_file", json!({"path": "a"})),
        ]
    );
    assert_eq!(response.provider_content.expect("content").blocks, blocks);
}

#[test]
fn stop_reasons_map() {
    let call = json!([tool_use("toolu_1", "list_dir", &json!({}))]);
    let say = json!([text("x")]);
    for (content, stop, expected) in [
        (&call, "tool_use", StopReason::ToolUse),
        (&say, "end_turn", StopReason::EndTurn),
        (&say, "stop_sequence", StopReason::EndTurn),
        (&say, "max_tokens", StopReason::MaxTokens),
        (&say, "model_context_window_exceeded", StopReason::MaxTokens),
    ] {
        assert_eq!(parsed(content, stop).stop_reason, expected, "{stop}");
    }
}

#[test]
fn a_refusal_keeps_its_stop_details_out_of_the_text() {
    let mut body = message(&json!([]), "refusal", &usage(100, 0));
    body["stop_details"] = json!({"type": "refusal", "category": "cyber",
                                  "explanation": "declined", "recommended_model": null});
    let response =
        parse_response(&body, "claude-opus-5-5", true).expect("a pre-output refusal is valid");
    assert_eq!(response.stop_reason, StopReason::Refusal);
    assert_eq!(response.text, None);
    let content = response.provider_content.expect("content");
    assert_eq!(content.blocks, json!([]));
    assert_eq!(
        content
            .stop_details
            .as_ref()
            .and_then(|d| d.get("category")),
        Some(&json!("cyber"))
    );

    body["stop_details"] = json!("cyber");
    let error = parse_response(&body, "claude-opus-5-5", true).expect_err("not an object");
    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
}

#[test]
fn unknown_and_fallback_blocks_are_kept_only_as_opaque_content() {
    let blocks = json!([
        // Nothing may come before the boundary: the declined partial is
        // omitted from a non-streaming response.
        {"type": "fallback", "from": {"model": "claude-opus-5-5"}, "to": {"model": "claude-opus-4-8"}},
        {"type": "redacted_thinking", "data": "opaque=="},
        {"type": "brand_new_block", "payload": {"nested": [1, -2, "three"]}},
        text("answer"),
    ]);
    // A fallback ran, so usage takes the documented fallback form (no live
    // fallback has been captured): the declined attempt, then the serving one.
    let mut fallback_usage = usage(100, 400);
    fallback_usage["iterations"] = json!([
        {"type": "message", "model": null, "input_tokens": 100, "output_tokens": 0},
        {"type": "fallback_message", "model": "claude-opus-4-8",
         "input_tokens": 100, "output_tokens": 400},
    ]);
    let mut body = message(&blocks, "end_turn", &fallback_usage);
    body["model"] = json!("claude-opus-4-8");
    let response = parse_response(&body, "claude-opus-5-5", true).expect("valid");
    assert_eq!(response.text.as_deref(), Some("answer"));
    assert!(response.tool_calls.is_empty());
    let content = response.provider_content.expect("content");
    assert_eq!(content.blocks, blocks);
    assert_eq!(content.model.as_deref(), Some("claude-opus-4-8"));
}

#[test]
fn malformed_blocks_and_tool_uses_are_invalid() {
    let long_name = "n".repeat(257);
    let cases = [
        (
            json!([tool_use("toolu_1", "list_dir", &json!("{}"))]),
            "input is not an object",
        ),
        (
            json!([tool_use("toolu_1", "list_dir", &json!(["a"]))]),
            "input is not an object",
        ),
        (
            json!([{"type": "tool_use", "id": "toolu_1", "name": "list_dir"}]),
            "input is not an object",
        ),
        (
            json!([tool_use("toolu 1", "list_dir", &json!({}))]),
            "call-ID rule",
        ),
        (
            json!([tool_use(&"t".repeat(65), "list_dir", &json!({}))]),
            "call-ID rule",
        ),
        (
            json!([{"type": "tool_use", "name": "list_dir", "input": {}}]),
            "call-ID rule",
        ),
        (
            json!([tool_use("toolu_1", &long_name, &json!({}))]),
            "tool name exceeds 256 bytes",
        ),
        (json!([{"type": "text"}]), "text block has no text"),
        (json!(["just a string"]), "not an object with a type"),
        (json!([{"no_type": true}]), "not an object with a type"),
    ];
    for (content, needle) in cases {
        let error = parse_response(
            &message(&content, "tool_use", &usage(100, 400)),
            "claude-opus-5-5",
            true,
        )
        .expect_err("invalid");
        assert_eq!(error.kind, ProviderErrorKind::InvalidResponse, "{content}");
        assert!(
            error.message.contains(needle),
            "{needle}: {}",
            error.message
        );
    }
    let mut no_content = message(&json!([]), "end_turn", &usage(1, 1));
    no_content
        .as_object_mut()
        .expect("object")
        .remove("content");
    assert!(parse_response(&no_content, "claude-opus-5-5", true).is_err());
    let mut bad_model = message(&json!([text("x")]), "end_turn", &usage(1, 1));
    bad_model["model"] = json!("claude\u{1b}[2J");
    assert!(
        parse_response(&bad_model, "claude-opus-5-5", true)
            .expect_err("model")
            .message
            .contains("model identifier")
    );
}

// 3. Round trip: response N's content is request N+1's assistant turn.

fn task() -> SessionTask {
    SessionTask::new(
        "Use the tools.".to_string(),
        "Write hello to a.txt, read it back, then report.".to_string(),
    )
}

fn session_script() -> Vec<Reply> {
    vec![
        ok(
            &json!([
                thinking(""),
                text("Writing the file."),
                tool_use(
                    "toolu_01W",
                    "write_file",
                    &json!({"path": "a.txt", "content": "hello\n"})
                ),
            ]),
            "tool_use",
        ),
        Reply::json(
            200,
            &message(
                &json!([
                    thinking(""),
                    tool_use("toolu_01R", "read_file", &json!({"path": "a.txt"})),
                    tool_use("toolu_01L", "list_dir", &json!({"path": "."})),
                ]),
                "tool_use",
                // The first turn's prefix is now cached.
                &common::anthropic::live_usage(30, 40, 900, 70),
            ),
        ),
        ok(
            &json!([thinking(""), text("a.txt contains hello.")]),
            "end_turn",
        ),
    ]
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one session checked end to end
async fn a_full_session_echoes_content_verifies_and_replays_without_the_server() {
    let temp = common::private_tempdir().expect("tempdir");
    let recorded = temp.path().join("run.jsonl");
    let script = session_script();
    let server = FakeHttp::start(script.clone()).await;
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        provider(config(&base(&server))),
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
            input_tokens: 230,
            output_tokens: 120,
            cache_read_input_tokens: 900,
            cache_creation_input_tokens: 70,
        }
    );
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let bodies = requests
        .iter()
        .map(common::http::RecordedRequest::json)
        .collect::<Vec<_>>();
    let responses = script
        .iter()
        .map(|reply| match reply {
            Reply::Respond { body, .. } => serde_json::from_slice::<Value>(body).expect("json"),
            other => panic!("{other:?}"),
        })
        .collect::<Vec<_>>();
    // Each response's content comes back byte-identical in the next request.
    for turn in 0..2 {
        let messages = bodies[turn + 1]["messages"].as_array().expect("messages");
        let echoed = &messages[1 + 2 * turn];
        assert_eq!(echoed["role"], "assistant");
        assert_eq!(
            serde_json::to_string(&echoed["content"]).expect("encode"),
            serde_json::to_string(&responses[turn]["content"]).expect("encode")
        );
    }
    // All results of a turn go in one user message, routed by tool_use ID.
    let last = bodies[2]["messages"].as_array().expect("messages");
    assert_eq!(last.len(), 5);
    let results = last[4]["content"].as_array().expect("results");
    let routed = results
        .iter()
        .map(|result| result["tool_use_id"].as_str().expect("id"))
        .collect::<Vec<_>>();
    assert_eq!(routed, ["toolu_01R", "toolu_01L"]);
    assert!(results.iter().all(|result| result["is_error"] == false));
    let read: Value =
        serde_json::from_str(results[0]["content"].as_str().expect("content")).expect("json");
    assert_eq!(read["content"], "hello\n");

    // The grammar rebuilds every request, opaque content included.
    let journal = read_journal(&recorded).expect("journal verifies");
    assert!(matches!(
        &journal.records[0].event,
        JournalEvent::SessionStarted { provider, .. }
            if provider.name == "anthropic" && provider.model == "claude-opus-5-5"
    ));
    let served_models = harness
        .audit()
        .records
        .iter()
        .filter_map(|record| match &record.event {
            AuditEvent::ModelResponded { served_model, .. } => served_model.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(served_models, ["claude-opus-5-5"; 3]);

    // The server is gone; replay needs only the journal and kilnd.
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

#[tokio::test]
async fn consecutive_requests_share_a_byte_identical_prefix() {
    let server = FakeHttp::start(session_script()).await;
    let mut harness = Harness::new(
        FakeApi::default(),
        provider(config(&base(&server))),
        MemoryAudit::default(),
        MemoryJournal::default(),
        short_lifecycle_config(),
    );
    harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("session completes");
    let requests = server.requests();
    for pair in requests.windows(2) {
        let (before, after) = (&pair[0], &pair[1]);
        let first = String::from_utf8(before.body.clone()).expect("utf-8");
        let second = String::from_utf8(after.body.clone()).expect("utf-8");
        // Everything up to the end of the earlier request's last message.
        let old = serde_json::to_string(&before.json()["messages"]).expect("encode");
        let at = first
            .find(&format!("\"messages\":{old}"))
            .expect("messages in the body");
        let prefix = at + "\"messages\":".len() + old.len() - 1;
        assert_eq!(&second[..prefix], &first[..prefix]);
        assert_eq!(&second[prefix..=prefix], ",", "a new message follows");
        // Tools and system, which render before the messages, are unchanged.
        for key in ["tools", "system", "tool_choice", "output_config", "model"] {
            assert_eq!(before.json()[key], after.json()[key], "{key}");
        }
    }
}

// 4. Usage.

#[test]
fn usage_is_disjoint_and_a_fully_cached_prompt_is_accepted() {
    let cached = common::anthropic::live_usage(0, 5, 4000, 12);
    let response = parse_response(
        &message(&json!([text("ok")]), "end_turn", &cached),
        "claude-opus-5-5",
        true,
    )
    .expect("fully cached");
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 0,
            output_tokens: 5,
            cache_read_input_tokens: 4000,
            cache_creation_input_tokens: 12,
        }
    );
    // Invented: missing and null counters, without iterations.
    let sparse = parse_response(
        &message(
            &json!([text("ok")]),
            "end_turn",
            &json!({"input_tokens": 9, "output_tokens": 1, "cache_read_input_tokens": null}),
        ),
        "claude-opus-5-5",
        false,
    )
    .expect("missing and null counters are zero");
    assert_eq!(sparse.usage.cache_read_input_tokens, 0);
}

#[test]
fn implausible_usage_is_invalid() {
    // Synthetic top-level-only usage, invented to isolate each check; it
    // is valid only without fallbacks, which make iterations required.
    let content = json!([text(&"x".repeat(650))]);
    let cases = [
        (
            json!({"input_tokens": 0, "output_tokens": 20}),
            "zero input tokens",
        ),
        (
            json!({"input_tokens": 10, "output_tokens": 0}),
            "zero completion tokens",
        ),
        (
            json!({"input_tokens": 10, "output_tokens": 10}),
            "10 completion tokens for 650 bytes",
        ),
        (
            json!({"input_tokens": -1, "output_tokens": 20}),
            "input_tokens is not a count",
        ),
        (
            json!({"input_tokens": 1.5, "output_tokens": 20}),
            "input_tokens is not a count",
        ),
    ];
    for (usage, needle) in cases {
        let error = parse_response(
            &message(&content, "end_turn", &usage),
            "claude-opus-5-5",
            false,
        )
        .expect_err("invalid");
        assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
        assert!(
            error.message.contains(needle),
            "{needle}: {}",
            error.message
        );
    }
    let mut no_usage = message(&content, "end_turn", &json!({}));
    no_usage.as_object_mut().expect("object").remove("usage");
    assert!(
        parse_response(&no_usage, "claude-opus-5-5", false)
            .expect_err("no usage")
            .message
            .contains("no usage")
    );
    // Opaque blocks alone are output too.
    let error = parse_response(
        &message(
            &json!([thinking("")]),
            "end_turn",
            &json!({"input_tokens": 10, "output_tokens": 0}),
        ),
        "claude-opus-5-5",
        false,
    )
    .expect_err("zero tokens for a thinking block");
    assert!(error.message.contains("zero completion tokens"), "{error}");
}

#[tokio::test]
async fn large_signatures_skip_the_token_floor_but_count_toward_the_byte_budget() {
    let signature = "s".repeat(200_000);
    let blocks = json!([
        {"type": "thinking", "thinking": "", "signature": signature},
        text("done"),
    ]);
    // One output token covers four bytes of text; the signature is free.
    let response = parse_response(
        &message(&blocks, "end_turn", &usage(10, 1)),
        "claude-opus-5-5",
        true,
    )
    .expect("the floor counts only the neutral view");
    assert_eq!(response.token_floor_bytes(), 4);
    assert!(response.output_bytes() > 200_000);

    let server = FakeHttp::start([Reply::json(
        200,
        &message(&blocks, "end_turn", &usage(10, 1)),
    )])
    .await;
    let mut config = short_lifecycle_config();
    config.max_total_model_output_bytes = 100_000;
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        provider(self::config(&base(&server))),
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    );
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("over the byte budget");
    assert!(
        matches!(error, HarnessError::OutputBytesBudget(100_000)),
        "{error}"
    );
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
}

// 5. Errors and retries.

#[tokio::test]
async fn rate_limits_honor_retry_after_then_succeed() {
    let server = FakeHttp::start([
        Reply::status(429, &error_body("rate_limit_error", "slow down"))
            .with_header("retry-after", "1"),
        ok(&json!([text("ok")]), "end_turn"),
    ])
    .await;
    let response = complete_once(&server).await.expect("second attempt");
    assert_eq!(response.text.as_deref(), Some("ok"));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    // retry-after 1 s is capped at the 40 ms max delay.
    let waited = requests[1].at - requests[0].at;
    assert!(waited >= Duration::from_millis(30), "{waited:?}");
    assert!(waited < Duration::from_millis(900), "{waited:?}");
}

#[tokio::test]
async fn overload_is_retried_until_attempts_run_out() {
    let busy = || Reply::status(529, &error_body("overloaded_error", "busy"));
    let server = FakeHttp::start([
        busy(),
        busy(),
        busy(),
        ok(&json!([text("late")]), "end_turn"),
    ])
    .await;
    let error = complete_once(&server)
        .await
        .expect_err("attempts exhausted");
    assert_eq!(error.kind, ProviderErrorKind::Overloaded);
    assert!(error.retryable);
    assert_eq!(
        error.message,
        "HTTP 529 overloaded_error, request req_011CTest42 (body withheld: authenticated request)"
    );
    assert_eq!(server.requests().len(), 3);

    let server = FakeHttp::start([
        Reply::status(500, &error_body("api_error", "internal")),
        ok(&json!([text("recovered")]), "end_turn"),
    ])
    .await;
    let response = complete_once(&server).await.expect("500 is retried");
    assert_eq!(response.text.as_deref(), Some("recovered"));
}

#[tokio::test]
async fn context_overflow_and_client_errors_are_not_retried() {
    let cases = [
        (
            400,
            error_body(
                "invalid_request_error",
                "prompt is too long: 1048577 tokens > 1048576 maximum",
            ),
            ProviderErrorKind::ContextOverflow,
            "invalid_request_error",
        ),
        (
            413,
            error_body("request_too_large", "too big"),
            ProviderErrorKind::ContextOverflow,
            "request_too_large",
        ),
        (
            401,
            error_body("authentication_error", "bad key"),
            ProviderErrorKind::Rejected,
            "authentication_error",
        ),
        (
            403,
            error_body("permission_error", "nope"),
            ProviderErrorKind::Rejected,
            "permission_error",
        ),
        (
            404,
            error_body("not_found_error", "no model"),
            ProviderErrorKind::Rejected,
            "not_found_error",
        ),
        (
            400,
            error_body("invalid_request_error", "bad"),
            ProviderErrorKind::Rejected,
            "invalid_request_error",
        ),
        (
            422,
            error_body("teapot_error", "?"),
            ProviderErrorKind::Rejected,
            "unknown",
        ),
        (
            400,
            "not json".to_string(),
            ProviderErrorKind::Rejected,
            "unknown",
        ),
    ];
    for (status, body, kind, label) in cases {
        let server = FakeHttp::start([
            Reply::status(status, &body),
            ok(&json!([text("x")]), "end_turn"),
        ])
        .await;
        let error = complete_once(&server).await.expect_err("fails at once");
        assert_eq!(error.kind, kind, "{status} {body}");
        assert!(!error.retryable);
        assert!(
            error.message.starts_with(&format!("HTTP {status} {label}")),
            "{}",
            error.message
        );
        for quoted in [
            "too long", "too big", "bad key", "nope", "no model", "not json", "teapot",
        ] {
            assert!(!error.message.contains(quoted), "{}", error.message);
        }
        assert_eq!(server.requests().len(), 1, "{status}");
    }
}

#[tokio::test]
async fn an_unvalidated_request_id_is_dropped() {
    let body = json!({"type": "error", "error": {"type": "api_error", "message": "x"},
                      "request_id": "req_\u{1b}[2J"})
    .to_string();
    let server = FakeHttp::start([Reply::status(400, &body)]).await;
    let error = complete_once(&server).await.expect_err("rejected");
    assert_eq!(
        error.message,
        "HTTP 400 api_error (body withheld: authenticated request)"
    );
}

#[tokio::test]
async fn an_error_in_a_success_body_is_not_quoted() {
    let server = FakeHttp::start([Reply::json(
        200,
        &json!({"type": "error", "error": {"type": "overloaded_error", "message": "secret detail"}}),
    )])
    .await;
    let error = complete_once(&server).await.expect_err("no content");
    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
    assert_eq!(
        error.message,
        "server reported overloaded_error (body withheld: authenticated request)"
    );
}

// 6. Credential hygiene.

/// `SENTINEL_KEY` with every character as a `\u` escape, so the raw body
/// does not contain it but the decoded string does.
fn escaped_key() -> String {
    use std::fmt::Write as _;
    let mut escaped = String::new();
    for character in SENTINEL_KEY.chars() {
        let _ = write!(escaped, "\\u{:04x}", u32::from(character));
    }
    escaped
}

#[tokio::test]
async fn an_echoed_key_in_any_block_is_refused() {
    let placements = [
        json!([{"type": "thinking", "thinking": "KEY", "signature": SIGNATURE}, text("x")]),
        json!([{"type": "thinking", "thinking": "", "signature": "KEY"}, text("x")]),
        json!([{"type": "fallback", "from": {"model": "KEY"}, "to": {"model": "m"}}, text("x")]),
        json!([tool_use("toolu_1", "list_dir", &json!({"path": "KEY"}))]),
        json!([tool_use("toolu_1", "list_dir", &json!({"KEY": "."}))]),
        json!([{"type": "unknown_block", "deep": [{"x": "prefix KEY suffix"}]}, text("x")]),
        json!([text("x")]),
    ];
    for (index, content) in placements.iter().enumerate() {
        let mut body = message(content, "end_turn", &usage(10, 10));
        if index == placements.len() - 1 {
            body["stop_details"] = json!({"type": "refusal", "explanation": "KEY"});
        }
        let template = body.to_string();
        for spelling in [SENTINEL_KEY.to_string(), escaped_key()] {
            let raw = template.replace("KEY", &spelling);
            let server = FakeHttp::start([Reply::Respond {
                status: 200,
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body: raw.into_bytes(),
                delay: Duration::ZERO,
            }])
            .await;
            let error = complete_once(&server).await.expect_err("echo refused");
            assert_eq!(error.kind, ProviderErrorKind::Rejected, "{content}");
            assert_eq!(error.message, CREDENTIAL_ECHO);
            assert!(!error.retryable);
        }
    }
}

#[tokio::test]
async fn the_key_never_reaches_the_journal_audit_or_errors() {
    let temp = common::private_tempdir().expect("tempdir");
    let audit_path = temp.path().join("audit.jsonl");
    let journal_path = temp.path().join("run.jsonl");
    let failed_path = temp.path().join("failed.jsonl");
    let server = FakeHttp::start(session_script().into_iter().chain([Reply::status(
        401,
        &error_body(
            "authentication_error",
            &format!("invalid x-api-key {SENTINEL_KEY}"),
        ),
    )]))
    .await;
    let keyed = config(&base(&server));
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
    assert!(
        error
            .to_string()
            .contains("HTTP 401 authentication_error, request req_011CTest42 (body withheld"),
        "{error}"
    );
    assert!(!format!("{error:?}").contains("SENTINEL"));
    for path in [&audit_path, &journal_path, &failed_path] {
        let text = std::fs::read_to_string(path).expect("read");
        assert!(!text.is_empty());
        assert!(
            !text.contains("SENTINEL"),
            "{} leaks the key",
            path.display()
        );
    }
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests {
        assert_eq!(request.header("x-api-key"), Some(SENTINEL_KEY));
        assert!(!String::from_utf8_lossy(&request.body).contains(SENTINEL_KEY));
    }
}

#[test]
fn https_is_required_except_for_loopback_ip_literals() {
    let key = || Some(SecretString::new(SENTINEL_KEY));
    for refused in [
        "http://api.example.com",
        "http://localhost:8080",
        "http://192.168.1.20",
        "ftp://api.anthropic.com",
        "https://user:pw@api.anthropic.com",
        "https://api.anthropic.com/?q=1",
    ] {
        let config = AnthropicConfig::new(refused, "claude-opus-5-5", key()).expect("parses");
        let error = config.validate().expect_err(refused);
        assert!(!error.to_string().contains(SENTINEL_KEY));
    }
    for accepted in [
        "https://api.anthropic.com",
        "http://127.0.0.1:9",
        "http://[::1]:9",
    ] {
        AnthropicConfig::new(accepted, "claude-opus-5-5", key())
            .expect("parses")
            .validate()
            .expect(accepted);
    }
    for bad_key in ["", "sk ant", "sk-\u{e9}"] {
        let config = AnthropicConfig::new(
            "https://api.anthropic.com",
            "m",
            Some(SecretString::new(bad_key)),
        )
        .expect("parses");
        assert!(config.validate().is_err(), "{bad_key:?}");
    }
}

// 7. The engine refuses what it must, whatever the provider.

#[tokio::test]
async fn a_refusal_ends_the_session_as_refused() {
    let mut body = message(&json!([]), "refusal", &usage(100, 0));
    body["stop_details"] = json!({"type": "refusal", "category": "cyber"});
    let server = FakeHttp::start([Reply::json(200, &body)]).await;
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("refused.jsonl");
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        provider(config(&base(&server))),
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        short_lifecycle_config(),
    );
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("refused");
    assert!(matches!(error, HarnessError::ModelRefused), "{error}");
    assert_eq!(api.state().deleted, ["sb-0000000000000001"]);
    let journal = read_journal(&path).expect("verifies");
    let recorded = journal
        .records
        .iter()
        .find_map(|record| match &record.event {
            JournalEvent::ModelResponded { response, .. } => response.provider_content.clone(),
            _ => None,
        });
    assert_eq!(
        recorded
            .and_then(|content| content.stop_details)
            .and_then(|details| details.get("category").cloned()),
        Some(json!("cyber"))
    );
}

// 8. Credential-proxy mode: no key in the harness at all.

fn keyless(base_url: &str) -> AnthropicConfig {
    let mut config = AnthropicConfig::new(base_url, "claude-opus-5-5", None).expect("config");
    config.retry_base_delay = Duration::from_millis(5);
    config.retry_max_delay = Duration::from_millis(40);
    config
}

#[tokio::test]
async fn a_keyless_request_sends_no_credential_header() {
    let server = FakeHttp::start([ok(&json!([text("via proxy")]), "end_turn")]).await;
    let config = keyless(&base(&server));
    assert!(format!("{config:?}").contains("api_key: None"));
    let provider = provider(config);
    assert!(format!("{provider:?}").contains("none: credential proxy"));
    let response = complete_with(keyless(&base(&server)))
        .await
        .expect("response");
    assert_eq!(response.text.as_deref(), Some("via proxy"));
    let request = &server.requests()[0];
    assert_eq!(request.header("x-api-key"), None);
    assert_eq!(request.header("authorization"), None);
    assert_eq!(request.header("anthropic-version"), Some(API_VERSION));
    assert_eq!(
        request.header("anthropic-beta"),
        None,
        "fallbacks are opt-in"
    );
}

#[test]
fn keyless_mode_allows_plain_http_but_not_the_api_itself() {
    for proxy in [
        "http://host.lima.internal:8788",
        "http://192.168.5.2:8788",
        "http://127.0.0.1:8788",
        "https://proxy.example.com",
    ] {
        keyless(proxy).validate().expect(proxy);
    }
    for direct in [
        "https://api.anthropic.com",
        "https://API.Anthropic.com./v1-root",
        "http://api.anthropic.com",
    ] {
        let error = keyless(direct).validate().expect_err(direct);
        assert!(
            error.to_string().contains("credential-injecting proxy"),
            "{error}"
        );
    }
    // A key still needs https off loopback, proxy or not.
    let error = config("http://host.lima.internal:8788")
        .validate()
        .expect_err("cleartext key");
    assert!(
        error.to_string().contains("requires an https base URL"),
        "{error}"
    );
}

#[tokio::test]
async fn keyless_errors_quote_a_bounded_sanitized_excerpt() {
    let relayed = format!(
        "{{\"type\":\"error\",\"error\":{{\"type\":\"rate_limit_error\",\
         \"message\":\"slow \\u001b[31mdown {}\"}},\"request_id\":\"req_7\"}}",
        "x".repeat(2000)
    );
    let server = FakeHttp::start([
        Reply::status(429, &relayed),
        Reply::status(401, "proxy: \u{1b}[2Jupstream refused"),
    ])
    .await;
    let mut config = keyless(&base(&server));
    config.max_attempts = 1;
    let error = complete_with(config.clone())
        .await
        .expect_err("rate limited");
    assert_eq!(error.kind, ProviderErrorKind::RateLimited);
    assert!(error.retryable);
    let message = &error.message;
    assert!(
        message.starts_with("HTTP 429 rate_limit_error, request req_7: {"),
        "{message}"
    );
    assert!(message.contains("slow"), "the excerpt is quoted: {message}");
    assert!(!message.chars().any(char::is_control), "{message}");
    assert!(message.len() < 512 + 64, "{}", message.len());

    let error = complete_with(config).await.expect_err("proxy refused");
    assert_eq!(error.kind, ProviderErrorKind::Rejected);
    assert_eq!(
        error.message,
        "HTTP 401 unknown: proxy: [2Jupstream refused"
    );

    let server = FakeHttp::start([Reply::json(
        200,
        &json!({"type": "error", "error": {"type": "overloaded_error", "message": "detail"}}),
    )])
    .await;
    let error = complete_with(keyless(&base(&server)))
        .await
        .expect_err("no content");
    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
    assert!(
        error
            .message
            .starts_with("server reported overloaded_error: {"),
        "{}",
        error.message
    );
    assert!(error.message.contains("detail"), "{}", error.message);
}

#[tokio::test]
async fn a_keyless_session_verifies_and_replays() {
    let temp = common::private_tempdir().expect("tempdir");
    let recorded = temp.path().join("proxy.jsonl");
    let server = FakeHttp::start(session_script()).await;
    let mut harness = Harness::new(
        FakeApi::default(),
        provider(keyless(&base(&server))),
        MemoryAudit::default(),
        JsonlJournal::open(&recorded).expect("journal"),
        short_lifecycle_config(),
    );
    let outcome = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("session completes");
    assert_eq!(outcome.final_answer, "a.txt contains hello.");
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.header("x-api-key").is_none())
    );
    server.stop();
    let journal = read_journal(&recorded).expect("journal verifies");
    let replay = ReplayProvider::from_journal(&journal).expect("replayable");
    let mut config = short_lifecycle_config();
    config.apply_limits(&replay.limits());
    let replay_task = replay.task();
    let mut harness = Harness::new(
        FakeApi::default(),
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
    assert!(harness.provider().exhausted());
}
