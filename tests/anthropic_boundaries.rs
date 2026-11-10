//! Regression tests for the first adversarial review of the anthropic
//! provider: fallback usage, credential-shaped error fields, inexact opaque
//! numbers, the success envelope and stop reasons, and strict tool schemas.
mod common;

use std::time::Duration;

use common::http::{FakeHttp, Reply};
use common::{FakeApi, short_lifecycle_config};
use kiln_harness::audit::{AuditEvent, MemoryAudit};
use kiln_harness::engine::{Harness, HarnessError, SessionTask};
use kiln_harness::journal::{JournalEvent, JsonlJournal, MemoryJournal, read_journal};
use kiln_harness::provider::{
    Message, ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind, Usage,
};
use kiln_harness::providers::anthropic::{parse_response, request_body, strict_input_schema};
use kiln_harness::providers::{AnthropicConfig, AnthropicProvider, SecretString};
use kiln_harness::tools::{ToolLimits, ToolRegistry};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const KEY: &str = "sk-ant-review-r1-key";

fn config_with(base_url: &str, key: &str) -> AnthropicConfig {
    let mut config =
        AnthropicConfig::new(base_url, "claude-opus-5-5", Some(SecretString::new(key)))
            .expect("config");
    config.retry_base_delay = Duration::from_millis(1);
    config.retry_max_delay = Duration::from_millis(1);
    config
}

fn base(server: &FakeHttp) -> String {
    format!("http://{}", server.addr)
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

fn usage(input: u64, output: u64) -> Value {
    json!({"input_tokens": input, "output_tokens": output})
}

fn raw(body: String) -> Reply {
    Reply::Respond {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: body.into_bytes(),
        delay: Duration::ZERO,
    }
}

fn task() -> SessionTask {
    SessionTask {
        system: String::new(),
        prompt: "answer".to_string(),
    }
}

fn harness_for(
    provider: AnthropicProvider,
    config: kiln_harness::engine::HarnessConfig,
) -> Harness<FakeApi, AnthropicProvider, MemoryAudit, MemoryJournal> {
    Harness::new(
        FakeApi::default(),
        provider,
        MemoryAudit::default(),
        MemoryJournal::default(),
        config,
    )
}

async fn complete(server: &FakeHttp, key: &str) -> Result<ModelResponse, ProviderError> {
    let mut provider = AnthropicProvider::new(config_with(&base(server), key)).expect("provider");
    let messages = [Message::User {
        text: "hi".to_string(),
    }];
    provider
        .complete(ModelRequest {
            session_id: "hs-review",
            turn: 1,
            system: "",
            tools: &[],
            messages: &messages,
            max_output_tokens: 64,
        })
        .await
}

fn invalid(body: &Value) -> String {
    let error = parse_response(body).expect_err("invalid");
    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse, "{body}");
    error.message
}

// D1: every server-side attempt counts.

/// The documented fallback shape: top-level usage is the serving attempt,
/// `usage.iterations` every attempt.
fn fallback_body(primary: &Value, fallback: &Value, top: &Value) -> Value {
    let mut usage = top.clone();
    usage["iterations"] = json!([
        {"type": "message", "model": "claude-opus-5-5", "input_tokens": primary[0],
         "output_tokens": primary[1], "cache_read_input_tokens": primary[2],
         "cache_creation_input_tokens": primary[3]},
        {"type": "fallback_message", "model": "claude-opus-4-8", "input_tokens": fallback[0],
         "output_tokens": fallback[1], "cache_read_input_tokens": fallback[2],
         "cache_creation_input_tokens": fallback[3]},
    ]);
    let mut body = message(
        &json!([
            {"type": "fallback", "from": {"model": "claude-opus-5-5"},
             "to": {"model": "claude-opus-4-8"}, "trigger": {"type": "refusal", "category": "cyber"}},
            {"type": "text", "text": "ok"}
        ]),
        "end_turn",
        &usage,
    );
    body["model"] = json!("claude-opus-4-8");
    body
}

#[tokio::test]
async fn fallback_attempts_count_against_the_session_budgets() {
    // Daybreak repro-01: 535/4 declined, then 412/2 served.
    let body = fallback_body(
        &json!([535, 4, 0, 0]),
        &json!([412, 2, 0, 0]),
        &usage(412, 2),
    );
    let server = FakeHttp::start([Reply::json(200, &body)]).await;
    let provider = AnthropicProvider::new(config_with(&base(&server), KEY)).expect("provider");
    let mut config = short_lifecycle_config();
    config.max_output_tokens_per_request = 10;
    config.max_total_input_tokens = 600;
    config.max_total_output_tokens = 5;
    let mut harness = harness_for(provider, config);
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("947 input and 6 output tokens exceed the budgets");
    assert!(matches!(error, HarnessError::TokenBudget), "{error}");

    let parsed = parse_response(&body).expect("valid");
    assert_eq!(
        parsed.usage,
        Usage {
            input_tokens: 947,
            output_tokens: 6,
            ..Usage::default()
        }
    );
}

#[tokio::test]
async fn the_per_request_cap_covers_every_attempt_of_one_call() {
    // Each attempt is under the 10-token cap; together they are not.
    let body = fallback_body(&json!([50, 6, 0, 0]), &json!([50, 6, 0, 0]), &usage(50, 6));
    let server = FakeHttp::start([Reply::json(200, &body)]).await;
    let provider = AnthropicProvider::new(config_with(&base(&server), KEY)).expect("provider");
    let mut config = short_lifecycle_config();
    config.max_output_tokens_per_request = 10;
    let mut harness = harness_for(provider, config);
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("12 output tokens in one call");
    assert!(matches!(error, HarnessError::TokenBudget), "{error}");
}

#[tokio::test]
async fn a_mid_output_decline_is_summed_into_journal_audit_and_outcome() {
    let body = fallback_body(
        &json!([30, 40, 900, 70]),
        &json!([25, 12, 880, 15]),
        &json!({"input_tokens": 25, "output_tokens": 12,
                "cache_read_input_tokens": 880, "cache_creation_input_tokens": 15}),
    );
    let expected = Usage {
        input_tokens: 55,
        output_tokens: 52,
        cache_read_input_tokens: 1780,
        cache_creation_input_tokens: 85,
    };
    let server = FakeHttp::start([Reply::json(200, &body)]).await;
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("fallback.jsonl");
    let provider = AnthropicProvider::new(config_with(&base(&server), KEY)).expect("provider");
    let mut harness = Harness::new(
        FakeApi::default(),
        provider,
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        short_lifecycle_config(),
    );
    let outcome = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("completes");
    assert_eq!(outcome.usage, expected);

    let audited = harness
        .audit()
        .records
        .iter()
        .find_map(|record| match &record.event {
            AuditEvent::ModelResponded {
                usage,
                served_model,
                ..
            } => Some((*usage, served_model.clone())),
            _ => None,
        })
        .expect("model_responded");
    assert_eq!(audited, (expected, Some("claude-opus-4-8".to_string())));

    let journal = read_journal(&path).expect("verifies");
    let mut recorded = Vec::new();
    for record in &journal.records {
        match &record.event {
            JournalEvent::ModelResponded { response, .. } => recorded.push(response.usage),
            JournalEvent::SessionEnded { usage, .. } => recorded.push(*usage),
            _ => {}
        }
    }
    assert_eq!(recorded, [expected, expected]);
}

#[test]
fn malformed_iterations_are_invalid() {
    let text = json!([{"type": "text", "text": "ok"}]);
    let with = |iterations: Value| {
        message(
            &text,
            "end_turn",
            &json!({"input_tokens": 10, "output_tokens": 2, "iterations": iterations}),
        )
    };
    for (iterations, needle) in [
        (
            json!([{"input_tokens": -1, "output_tokens": 2}]),
            "iterations[0] input_tokens",
        ),
        (
            json!([{"input_tokens": 10, "output_tokens": 2}, {"output_tokens": 1.5}]),
            "iterations[1] output_tokens",
        ),
        (
            json!([{"input_tokens": 10, "cache_read_input_tokens": "9"}]),
            "iterations[0] cache_read_input_tokens",
        ),
        (json!([7]), "iterations[0] is not an object"),
        (json!([]), "not a non-empty array"),
        (json!({"input_tokens": 1}), "not a non-empty array"),
        (
            json!([{"input_tokens": 0, "output_tokens": 2}]),
            "zero input tokens",
        ),
        (
            json!([{"input_tokens": 10, "output_tokens": 0}]),
            "zero completion tokens",
        ),
    ] {
        let message = invalid(&with(iterations));
        assert!(message.contains(needle), "{needle}: {message}");
    }
    // The floor runs on the sum: 3 + 2 tokens cover 300 bytes of text.
    let long = json!([{"type": "text", "text": "x".repeat(300)}]);
    let summed = message(
        &long,
        "end_turn",
        &json!({"input_tokens": 1, "output_tokens": 2, "iterations": [
            {"input_tokens": 1, "output_tokens": 3}, {"input_tokens": 1, "output_tokens": 2}]}),
    );
    assert_eq!(
        parse_response(&summed).expect("valid").usage.output_tokens,
        5
    );
}

// D2: authenticated error fields never carry the key.

#[tokio::test]
async fn a_credential_shaped_request_id_is_dropped() {
    // Daybreak repro-02: a key that is also a valid request ID.
    let key = "req_DaybreakLeak42";
    let body = json!({"type": "error", "error": {"type": "authentication_error",
                      "message": "body text is withheld"}, "request_id": key});
    let server = FakeHttp::start([Reply::json(401, &body)]).await;
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("run.jsonl");
    let provider = AnthropicProvider::new(config_with(&base(&server), key)).expect("provider");
    let mut harness = Harness::new(
        FakeApi::default(),
        provider,
        MemoryAudit::default(),
        JsonlJournal::open(&path).expect("journal"),
        short_lifecycle_config(),
    );
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("rejected");
    let HarnessError::Provider(error) = error else {
        panic!("{error}");
    };
    assert_eq!(
        error.message,
        "HTTP 401 (body withheld: authenticated request)"
    );
    assert_eq!(server.requests()[0].header("x-api-key"), Some(key));
    let journal = std::fs::read_to_string(&path).expect("journal");
    assert!(!journal.contains(key));
    assert!(!journal.contains("DaybreakLeak"));
}

#[tokio::test]
async fn a_key_spelled_like_an_error_type_is_not_quoted() {
    let key = "rate_limit_error";
    let body = json!({"type": "error", "error": {"type": "rate_limit_error", "message": "x"},
                      "request_id": "req_ok1"});
    let server = FakeHttp::start([Reply::json(429, &body)]).await;
    let mut config = config_with(&base(&server), key);
    config.max_attempts = 1;
    let mut provider = AnthropicProvider::new(config).expect("provider");
    let messages = [Message::User {
        text: "hi".to_string(),
    }];
    let error = provider
        .complete(ModelRequest {
            session_id: "hs-review",
            turn: 1,
            system: "",
            tools: &[],
            messages: &messages,
            max_output_tokens: 8,
        })
        .await
        .expect_err("rate limited");
    assert_eq!(error.kind, ProviderErrorKind::RateLimited);
    assert_eq!(
        error.message,
        "HTTP 429 (body withheld: authenticated request)"
    );

    // An ordinary key keeps both fields.
    let server = FakeHttp::start([
        Reply::json(429, &body),
        Reply::json(429, &body),
        Reply::json(429, &body),
    ])
    .await;
    let error = complete(&server, KEY).await.expect_err("rate limited");
    assert_eq!(
        error.message,
        "HTTP 429 rate_limit_error, request req_ok1 (body withheld: authenticated request)"
    );
}

// D3: opaque numbers must echo exactly, or the response is refused.

fn opaque_number_body(number: &str) -> String {
    format!(
        r#"{{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[{{"type":"future_block","nonce":{number}}},{{"type":"tool_use","id":"toolu_1","name":"list_dir","input":{{"path":"."}}}}],"stop_reason":"tool_use","stop_details":null,"usage":{{"input_tokens":100,"output_tokens":10}}}}"#
    )
}

fn final_text() -> Reply {
    Reply::json(
        200,
        &message(
            &json!([{"type": "text", "text": "done"}]),
            "end_turn",
            &usage(100, 1),
        ),
    )
}

#[tokio::test]
async fn an_opaque_number_beyond_64_bits_is_refused() {
    // Daybreak repro-03: a 30-digit integer would come back as a float.
    let server = FakeHttp::start([
        raw(opaque_number_body("123456789012345678901234567890")),
        final_text(),
    ])
    .await;
    let provider = AnthropicProvider::new(config_with(&base(&server), KEY)).expect("provider");
    let mut harness = harness_for(provider, short_lifecycle_config());
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("refused");
    let HarnessError::Provider(error) = error else {
        panic!("{error}");
    };
    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
    assert_eq!(
        error.message,
        "opaque content contains a number that cannot be echoed exactly"
    );
    assert_eq!(server.requests().len(), 1, "nothing is echoed");
}

#[tokio::test]
async fn sixty_four_bit_integers_echo_exactly() {
    for number in ["18446744073709551615", "-9223372036854775808", "0"] {
        let server = FakeHttp::start([raw(opaque_number_body(number)), final_text()]).await;
        let provider = AnthropicProvider::new(config_with(&base(&server), KEY)).expect("provider");
        let mut harness = harness_for(provider, short_lifecycle_config());
        harness
            .run(&task(), CancellationToken::new())
            .await
            .expect("completes");
        let second = String::from_utf8(server.requests()[1].body.clone()).expect("utf-8");
        assert!(
            second.contains(&format!("\"nonce\":{number}")),
            "{number} echoed exactly: {second}"
        );
    }
}

#[test]
fn floats_in_opaque_content_are_refused_but_not_in_tool_input() {
    for number in [
        "1.5",
        "1e3",
        "-0.0",
        "18446744073709551616",
        "-9223372036854775809",
    ] {
        let body: Value = serde_json::from_str(&opaque_number_body(number)).expect("json");
        assert_eq!(
            invalid(&body),
            "opaque content contains a number that cannot be echoed exactly",
            "{number}"
        );
    }
    let mut refusal = message(&json!([]), "refusal", &usage(10, 0));
    refusal["stop_details"] = json!({"type": "refusal", "score": 0.5});
    assert!(invalid(&refusal).contains("cannot be echoed exactly"));
    // tool_use.input is the neutral arguments; the typed tool judges them.
    let call = message(
        &json!([{"type": "tool_use", "id": "toolu_1", "name": "exec",
                 "input": {"argv": ["/bin/true"], "timeout_ms": 1.5}}]),
        "tool_use",
        &usage(10, 10),
    );
    let parsed = parse_response(&call).expect("tool input floats pass");
    assert_eq!(parsed.tool_calls[0].arguments["timeout_ms"], json!(1.5));
}

// D4: only a real message with a known stop reason is a response.

#[tokio::test]
async fn an_error_envelope_with_content_is_refused() {
    // Daybreak repro-04.
    let body = json!({
        "type": "error",
        "error": {"type": "api_error", "message": "not a message"},
        "role": "assistant",
        "content": [{"type": "text", "text": "accepted error body"}],
        "stop_reason": "future_safety_stop",
        "usage": {"input_tokens": 10, "output_tokens": 1}
    });
    let server = FakeHttp::start([Reply::json(200, &body)]).await;
    let provider = AnthropicProvider::new(config_with(&base(&server), KEY)).expect("provider");
    let mut harness = harness_for(provider, short_lifecycle_config());
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("refused");
    let HarnessError::Provider(error) = error else {
        panic!("{error}");
    };
    assert_eq!(
        error.message,
        "server reported api_error (body withheld: authenticated request)"
    );
}

#[test]
fn the_envelope_and_stop_reason_are_required() {
    let good = message(
        &json!([{"type": "text", "text": "ok"}]),
        "end_turn",
        &usage(10, 1),
    );
    parse_response(&good).expect("the baseline is valid");
    let mut error_without_content = json!({"type": "error",
        "error": {"type": "overloaded_error", "message": "m"}});
    assert!(invalid(&error_without_content).contains("server reported overloaded_error"));
    error_without_content["content"] = json!([{"type": "text", "text": "ok"}]);
    assert!(invalid(&error_without_content).contains("server reported overloaded_error"));

    for (field, needle) in [
        ("type", "type is not \"message\""),
        ("role", "role is not \"assistant\""),
        ("model", "has no model"),
        ("content", "no content array"),
        ("stop_reason", "no stop_reason"),
        ("usage", "no usage"),
    ] {
        let mut body = good.clone();
        body.as_object_mut().expect("object").remove(field);
        assert!(invalid(&body).contains(needle), "{field}");
    }
    for (field, value, needle) in [
        ("type", json!("completion"), "type is not \"message\""),
        ("role", json!("user"), "role is not \"assistant\""),
        ("model", json!(null), "has no model"),
        ("model", json!("claude opus"), "not a model identifier"),
        ("stop_reason", json!(null), "no stop_reason"),
        (
            "stop_reason",
            json!("future_safety_stop"),
            "unknown stop_reason",
        ),
        (
            "stop_reason",
            json!("pause_turn"),
            "pause_turn needs server tools",
        ),
    ] {
        let mut body = good.clone();
        body[field] = value;
        assert!(
            invalid(&body).contains(needle),
            "{field}: {}",
            invalid(&body)
        );
    }
}

// D5: strict tools send schemas strict mode accepts.

#[test]
fn strict_schemas_drop_unsupported_keywords() {
    let registry = ToolRegistry::new(ToolLimits::default());
    let tools = registry.definitions();
    let exec = tools.iter().find(|tool| tool.name == "exec").expect("exec");
    assert_eq!(
        strict_input_schema(&exec.input_schema).expect("strict"),
        json!({
            "type": "object",
            "required": ["argv"],
            "additionalProperties": false,
            "properties": {
                "argv": {"type": "array", "items": {"type": "string"}},
                "stdin": {"type": "string"},
                "timeout_ms": {"type": "integer"}
            }
        })
    );
    // The canonical definition still carries every bound.
    assert_eq!(exec.input_schema["properties"]["argv"]["maxItems"], 128);
    let diff = tools
        .iter()
        .find(|tool| tool.name == "workspace_diff")
        .expect("diff");
    assert_eq!(
        strict_input_schema(&diff.input_schema).expect("strict")["required"],
        json!([])
    );
    for tool in tools {
        let strict = strict_input_schema(&tool.input_schema).expect(&tool.name);
        let text = strict.to_string();
        for keyword in ["maxItems", "minItems", "minimum", "maxLength", "pattern"] {
            assert!(!text.contains(keyword), "{}: {text}", tool.name);
        }
    }

    // Keywords go at any depth; a property named like one stays.
    let nested = json!({
        "type": "object", "additionalProperties": false, "required": ["pattern"],
        "properties": {
            "pattern": {"type": "string", "pattern": "^a", "maxLength": 9},
            "list": {"type": "array", "maxItems": 2, "items": {
                "type": "object", "additionalProperties": false,
                "properties": {"n": {"type": "number", "exclusiveMinimum": 0, "maximum": 9}}}},
            "either": {"anyOf": [{"type": "string", "minLength": 1}, {"type": "integer", "minimum": 0}]}
        }
    });
    assert_eq!(
        strict_input_schema(&nested).expect("strict"),
        json!({
            "type": "object", "additionalProperties": false, "required": ["pattern"],
            "properties": {
                "pattern": {"type": "string"},
                "list": {"type": "array", "items": {
                    "type": "object", "additionalProperties": false, "required": [],
                    "properties": {"n": {"type": "number"}}}},
                "either": {"anyOf": [{"type": "string"}, {"type": "integer"}]}
            }
        })
    );
    let open = json!({"type": "object", "properties": {"inner": {"type": "object"}},
                      "additionalProperties": false});
    let error = strict_input_schema(&open).expect_err("inner object is open");
    assert!(error.contains("input_schema.properties.inner"), "{error}");
}

#[test]
fn strict_mode_changes_only_the_request_schemas() {
    let registry = ToolRegistry::new(ToolLimits::default());
    let tools = registry.definitions();
    let messages = [Message::User {
        text: "hi".to_string(),
    }];
    let request = ModelRequest {
        session_id: "hs-review",
        turn: 1,
        system: "s",
        tools,
        messages: &messages,
        max_output_tokens: 8,
    };
    let mut config = config_with("https://api.anthropic.com", KEY);
    let loose = request_body(&config, &request).expect("body");
    for (sent, tool) in loose["tools"].as_array().expect("tools").iter().zip(tools) {
        assert_eq!(sent["input_schema"], tool.input_schema, "unchanged");
        assert!(sent.get("strict").is_none());
    }
    config.strict_tools = true;
    AnthropicProvider::new(config.clone()).expect("our schemas can be strict");
    let strict = request_body(&config, &request).expect("body");
    for (sent, tool) in strict["tools"].as_array().expect("tools").iter().zip(tools) {
        assert_eq!(sent["strict"], true);
        assert_eq!(
            sent["input_schema"],
            strict_input_schema(&tool.input_schema).expect("strict")
        );
    }
    let mut without_tools = loose.clone();
    let mut strict_without_tools = strict.clone();
    without_tools
        .as_object_mut()
        .expect("object")
        .remove("tools");
    strict_without_tools
        .as_object_mut()
        .expect("object")
        .remove("tools");
    assert_eq!(without_tools, strict_without_tools);
}

#[tokio::test]
async fn strict_requests_keep_a_stable_prefix() {
    let call = |id: &str| {
        message(
            &json!([{"type": "tool_use", "id": id, "name": "list_dir", "input": {"path": "."}}]),
            "tool_use",
            &usage(100, 20),
        )
    };
    let server = FakeHttp::start([
        Reply::json(200, &call("toolu_1")),
        Reply::json(200, &call("toolu_2")),
        final_text(),
    ])
    .await;
    let mut config = config_with(&base(&server), KEY);
    config.strict_tools = true;
    let provider = AnthropicProvider::new(config).expect("provider");
    let mut harness = harness_for(provider, short_lifecycle_config());
    harness
        .run(
            &SessionTask {
                system: "Use the tools.".to_string(),
                prompt: "List twice.".to_string(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("completes");
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    for pair in requests.windows(2) {
        let first = String::from_utf8(pair[0].body.clone()).expect("utf-8");
        let second = String::from_utf8(pair[1].body.clone()).expect("utf-8");
        let old = serde_json::to_string(&pair[0].json()["messages"]).expect("encode");
        let at = first
            .find(&format!("\"messages\":{old}"))
            .expect("messages");
        let prefix = at + "\"messages\":".len() + old.len() - 1;
        assert_eq!(&second[..prefix], &first[..prefix]);
        assert_eq!(pair[0].json()["tools"], pair[1].json()["tools"]);
        assert!(pair[1].json()["tools"][0]["strict"] == true);
    }
}
