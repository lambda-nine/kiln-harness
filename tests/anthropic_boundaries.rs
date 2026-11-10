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

/// The live non-fallback usage shape; see `common::anthropic`.
fn usage(input: u64, output: u64) -> Value {
    common::anthropic::live_usage(input, output, 0, 0)
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

fn invalid(body: &Value) -> String {
    let error = parse_response(body, "claude-opus-5-5").expect_err("invalid");
    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse, "{body}");
    error.message
}

// D1: every server-side attempt counts.

/// The documented fallback shape: top-level usage is the serving attempt,
/// `usage.iterations` every attempt. No live fallback has been captured, so
/// this follows the documentation, not a real response.
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

    let parsed = parse_response(&body, "claude-opus-5-5").expect("valid");
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
    // Fallback usage follows the documentation; no live fallback has been
    // captured.
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
    // Hand-built entries: each varies one field of the documented shape.
    let text = json!([{"type": "text", "text": "ok"}]);
    let entry = |tag: &str, model: &str, input: Value, output: Value| json!({"type": tag, "model": model, "input_tokens": input, "output_tokens": output});
    // Top level 10/2 from claude-opus-5-5; a valid last entry matches it.
    let serving = || entry("message", "claude-opus-5-5", json!(10), json!(2));
    let with = |iterations: Value| {
        message(
            &text,
            "end_turn",
            &json!({"input_tokens": 10, "output_tokens": 2, "iterations": iterations}),
        )
    };
    parse_response(&with(json!([serving()])), "claude-opus-5-5")
        .expect("a lone serving entry is valid");
    for (iterations, needle) in [
        (
            json!([entry("message", "m", json!(-1), json!(2)), serving()]),
            "iterations[0] input_tokens",
        ),
        (
            json!([entry("message", "m", json!(5), json!(1.5)), serving()]),
            "iterations[0] output_tokens",
        ),
        (
            json!([{"type": "message", "model": "m", "input_tokens": 5,
                 "cache_read_input_tokens": "9"}, serving()]),
            "iterations[0] cache_read_input_tokens",
        ),
        (json!([7, serving()]), "iterations[0] is not an object"),
        (
            json!([{"model": "m", "input_tokens": 5}, serving()]),
            "iterations[0] has no known type",
        ),
        (
            json!([entry("compaction", "m", json!(5), json!(1)), serving()]),
            "iterations[0] has no known type",
        ),
        (
            json!([{"type": "fallback_message", "input_tokens": 5}, serving()]),
            "iterations[0] has no valid model",
        ),
        (
            json!([entry("message", "bad model", json!(5), json!(1)), serving()]),
            "iterations[0] has no valid model",
        ),
        (
            json!([entry("message", "m", json!(0), json!(1)), serving()]),
            "iterations[0] reports zero input tokens",
        ),
        (json!([]), "not a non-empty array"),
        (json!({"input_tokens": 1}), "not a non-empty array"),
    ] {
        let message = invalid(&with(iterations));
        assert!(message.contains(needle), "{needle}: {message}");
    }
}

#[test]
fn iterations_must_end_with_the_serving_attempt() {
    // Fallback usage follows the documentation; no live fallback has been
    // captured.
    let mismatch = "usage iterations do not end with the serving attempt the top level reports";
    let text = json!([{"type": "text", "text": "ok"}]);
    let body = |top: Value, iterations: Value| {
        let mut usage = top;
        usage["iterations"] = iterations;
        let mut body = message(&text, "end_turn", &usage);
        body["model"] = json!("claude-opus-4-8");
        body
    };
    let declined = json!({"type": "message", "model": "claude-opus-5-5",
                          "input_tokens": 1, "output_tokens": 3});
    // Daybreak r2-repro-01: 900/500 at the top level, 1/1 in the last entry.
    let contradiction = body(
        json!({"input_tokens": 900, "output_tokens": 500}),
        json!([declined, {"type": "fallback_message", "model": "claude-opus-4-8",
                          "input_tokens": 1, "output_tokens": 1}]),
    );
    assert_eq!(invalid(&contradiction), mismatch);
    // The top level is validated even when iterations exist.
    let typed = body(
        json!({"input_tokens": "900", "output_tokens": 1}),
        json!([declined, {"type": "fallback_message", "model": "claude-opus-4-8",
                          "input_tokens": 900, "output_tokens": 1}]),
    );
    assert!(invalid(&typed).contains("usage input_tokens is not a count"));
    // The last entry must be the serving model.
    let wrong_model = body(
        json!({"input_tokens": 5, "output_tokens": 1}),
        json!([declined, {"type": "fallback_message", "model": "claude-opus-5",
                          "input_tokens": 5, "output_tokens": 1}]),
    );
    assert_eq!(invalid(&wrong_model), mismatch);
    // A serving entry that is there but not last does not count.
    let not_last = body(
        json!({"input_tokens": 5, "output_tokens": 1}),
        json!([{"type": "fallback_message", "model": "claude-opus-4-8",
                "input_tokens": 5, "output_tokens": 1}, declined]),
    );
    assert_eq!(invalid(&not_last), mismatch);
    // Only the declined attempt, no serving entry.
    let missing = body(
        json!({"input_tokens": 5, "output_tokens": 1}),
        json!([declined]),
    );
    assert_eq!(invalid(&missing), mismatch);
    // Cache counters must match exactly too.
    let cache = body(
        json!({"input_tokens": 5, "output_tokens": 1, "cache_read_input_tokens": 7}),
        json!([declined, {"type": "fallback_message", "model": "claude-opus-4-8",
                          "input_tokens": 5, "output_tokens": 1, "cache_read_input_tokens": 8}]),
    );
    assert_eq!(invalid(&cache), mismatch);
}

#[tokio::test]
async fn contradictory_serving_usage_cannot_complete_a_session() {
    // Daybreak r2-repro-01 end to end: refused before any budget is checked.
    let body = fallback_body(&json!([1, 3, 0, 0]), &json!([1, 1, 0, 0]), &usage(900, 500));
    let server = FakeHttp::start([Reply::json(200, &body)]).await;
    let provider = AnthropicProvider::new(config_with(&base(&server), KEY)).expect("provider");
    let mut config = short_lifecycle_config();
    config.max_total_input_tokens = 10;
    config.max_total_output_tokens = 10;
    let mut harness = harness_for(provider, config);
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("refused");
    let HarnessError::Provider(error) = error else {
        panic!("{error}");
    };
    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
}

#[test]
fn the_token_floor_uses_the_serving_output() {
    // Fallback usage follows the documentation; no live fallback has been
    // captured.
    // 300 bytes of returned text: the serving attempt must account for them.
    let long = json!([{"type": "text", "text": "x".repeat(300)}]);
    let body = |serving: u64| {
        message(
            &long,
            "end_turn",
            &json!({"input_tokens": 1, "output_tokens": serving, "iterations": [
                {"type": "message", "model": "claude-opus-5-5", "input_tokens": 1, "output_tokens": 50},
                {"type": "fallback_message", "model": "claude-opus-5-5",
                 "input_tokens": 1, "output_tokens": serving}]}),
        )
    };
    // 2 serving tokens cannot cover 300 bytes, whatever the declined attempt spent.
    assert!(invalid(&body(2)).contains("2 completion tokens for 300 bytes"));
    let accepted = parse_response(&body(5), "claude-opus-5-5").expect("5 tokens cover 300 bytes");
    assert_eq!(accepted.usage.output_tokens, 55, "budgets see the sum");
}

// D2 and E2: authenticated errors never carry the key.

#[tokio::test]
async fn a_credential_shaped_request_id_is_dropped() {
    // Daybreak repro-02: a key that is also a valid request ID.
    let key = "req_DaybreakLeak4242";
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

async fn complete_once(server: &FakeHttp, key: &str) -> Result<ModelResponse, ProviderError> {
    let mut config = config_with(&base(server), key);
    config.max_attempts = 1;
    let mut provider = AnthropicProvider::new(config).expect("provider");
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
            max_output_tokens: 8,
        })
        .await
}

#[tokio::test]
async fn a_key_spelled_like_an_error_type_is_not_quoted() {
    let key = "authentication_error";
    let body = json!({"type": "error", "error": {"type": key, "message": "x"},
                      "request_id": "req_ok1"});
    let server = FakeHttp::start([Reply::json(401, &body)]).await;
    let error = complete_once(&server, key).await.expect_err("rejected");
    assert_eq!(error.kind, ProviderErrorKind::Rejected);
    assert_eq!(
        error.message,
        "HTTP 401 (body withheld: authenticated request)"
    );

    // An ordinary key keeps both fields.
    let server = FakeHttp::start([Reply::json(401, &body)]).await;
    let error = complete_once(&server, KEY).await.expect_err("rejected");
    assert_eq!(
        error.message,
        "HTTP 401 authentication_error, request req_ok1 (body withheld: authenticated request)"
    );
}

#[tokio::test]
async fn a_key_completed_by_fixed_wording_empties_the_message() {
    // Daybreak r2-repro-02, at the 20-byte floor: the key is the end of the
    // allowlisted type plus the formatter's comma. Neither field holds it,
    // and the body does not either; only the finished message does.
    let key = "uthentication_error,";
    assert_eq!(key.len(), 20);
    let body = json!({"type": "error", "error": {"type": "authentication_error", "message": "x"},
                      "request_id": "req_safe1"});
    assert!(!body.to_string().contains(key));
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
    assert_eq!(error.kind, ProviderErrorKind::Rejected, "the kind is kept");
    assert_eq!(error.message, "");
    let journal = std::fs::read_to_string(&path).expect("journal");
    assert!(!journal.contains(key), "{journal}");
}

#[tokio::test]
async fn a_key_named_in_a_local_error_empties_it() {
    // A tool name is local text; a request error that names it is scrubbed
    // like any other.
    let key = "a_tool_named_like_the_key";
    let server = FakeHttp::start([]).await;
    let mut config = config_with(&base(&server), key);
    config.strict_tools = true;
    let mut provider = AnthropicProvider::new(config).expect("provider");
    let tools = [kiln_harness::provider::ToolDefinition {
        name: key.to_string(),
        description: "open".to_string(),
        input_schema: json!({"type": "object", "properties": {}}),
    }];
    let messages = [Message::User {
        text: "hi".to_string(),
    }];
    let error = provider
        .complete(ModelRequest {
            session_id: "hs-review",
            turn: 1,
            system: "",
            tools: &tools,
            messages: &messages,
            max_output_tokens: 8,
        })
        .await
        .expect_err("cannot be strict");
    assert_eq!(error.kind, ProviderErrorKind::Rejected);
    assert_eq!(error.message, "");
    assert_eq!(server.state().connections, 0);
}

#[test]
fn short_keys_are_refused_but_keyless_mode_is_not() {
    for short in ["HTTP", "sk-ant-19-bytes-key"] {
        let error = AnthropicConfig::new(
            "https://api.anthropic.com",
            "claude-opus-5-5",
            Some(SecretString::new(short)),
        )
        .expect("parses")
        .validate()
        .expect_err(short);
        assert_eq!(
            error.to_string(),
            "invalid anthropic configuration: API key looks truncated: under 20 bytes"
        );
    }
    config_with("https://api.anthropic.com", "sk-ant-20-bytes-key!")
        .validate()
        .expect("20 bytes");
    AnthropicConfig::new("http://127.0.0.1:9", "claude-opus-5-5", None)
        .expect("parses")
        .validate()
        .expect("keyless");
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
fn inexact_numbers_are_refused_anywhere_in_opaque_content() {
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
    // tool_use.input is echoed from the blocks too, so it is checked too.
    for number in ["1.5", "18446744073709551616"] {
        let body = format!(
            r#"{{"type":"message","role":"assistant","model":"claude-opus-5-5","content":[{{"type":"tool_use","id":"toolu_1","name":"exec","input":{{"argv":["/bin/true"],"timeout_ms":{number}}}}}],"stop_reason":"tool_use","usage":{{"input_tokens":10,"output_tokens":10}}}}"#
        );
        let body: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(
            invalid(&body),
            "opaque content contains a number that cannot be echoed exactly",
            "{number}"
        );
    }
}

#[tokio::test]
async fn a_u64_max_tool_argument_echoes_exactly() {
    let call = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_1","name":"read_file","input":{"path":"a.txt","max_bytes":18446744073709551615}}],"stop_reason":"tool_use","usage":{"input_tokens":100,"output_tokens":10}}"#;
    let server = FakeHttp::start([raw(call.to_string()), final_text()]).await;
    let provider = AnthropicProvider::new(config_with(&base(&server), KEY)).expect("provider");
    let mut harness = harness_for(provider, short_lifecycle_config());
    harness
        .run(&task(), CancellationToken::new())
        .await
        .expect("completes");
    let second = String::from_utf8(server.requests()[1].body.clone()).expect("utf-8");
    assert!(
        second.contains(r#""max_bytes":18446744073709551615"#),
        "echoed exactly: {second}"
    );
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
    parse_response(&good, "claude-opus-5-5").expect("the baseline is valid");
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
fn strict_schemas_strip_only_enforced_bounds() {
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
                "argv": {"type": "array", "minItems": 1, "items": {"type": "string"}},
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
        for keyword in ["maxItems", "minimum", "maximum"] {
            assert!(!text.contains(keyword), "{}: {text}", tool.name);
        }
    }

    // Bounds go at any depth; minItems 0 or 1 stays; a property named like
    // a keyword stays.
    let nested = json!({
        "type": "object", "additionalProperties": false, "required": ["maxItems"],
        "properties": {
            "maxItems": {"type": "string", "maxLength": 9},
            "list": {"type": "array", "minItems": 2, "maxItems": 4, "items": {
                "type": "object", "additionalProperties": false,
                "properties": {"n": {"type": "number", "exclusiveMinimum": 0, "maximum": 9}}}},
            "maybe": {"type": "array", "minItems": 0, "items": {"type": "string"}},
            "either": {"anyOf": [{"type": "string", "minLength": 1}, {"type": "integer", "minimum": 0}]}
        }
    });
    assert_eq!(
        strict_input_schema(&nested).expect("strict"),
        json!({
            "type": "object", "additionalProperties": false, "required": ["maxItems"],
            "properties": {
                "maxItems": {"type": "string"},
                "list": {"type": "array", "items": {
                    "type": "object", "additionalProperties": false, "required": [],
                    "properties": {"n": {"type": "number"}}}},
                "maybe": {"type": "array", "minItems": 0, "items": {"type": "string"}},
                "either": {"anyOf": [{"type": "string"}, {"type": "integer"}]}
            }
        })
    );
}

#[test]
fn unsupported_constructs_are_refused_with_their_path() {
    let object = |properties: Value| json!({"type": "object", "additionalProperties": false, "properties": properties});
    for (schema, expected) in [
        // Daybreak r2-repro-04's custom bound.
        (
            object(json!({"n": {"type": "integer", "multipleOf": 2}})),
            "input_schema.properties.n.multipleOf is not supported in strict mode",
        ),
        (
            object(json!({"node": {"$ref": "#/$defs/node"}})),
            "input_schema.properties.node.$ref is not supported in strict mode",
        ),
        (
            json!({"type": "object", "additionalProperties": false, "properties": {},
                   "$defs": {"node": {"type": "string"}}}),
            "input_schema.$defs is not supported in strict mode",
        ),
        (
            object(json!({"name": {"type": "string", "pattern": "^[a-z]+$"}})),
            "input_schema.properties.name.pattern is not supported in strict mode",
        ),
        (
            object(json!({"when": {"type": "string", "format": "date-time"}})),
            "input_schema.properties.when.format is not supported in strict mode",
        ),
        (
            object(json!({"pair": {"type": "array", "items": [{"type": "string"}]}})),
            "input_schema.properties.pair.items is not a schema",
        ),
        (
            object(json!({"inner": {"type": "object"}})),
            "input_schema.properties.inner is an object schema without \"additionalProperties\": false",
        ),
    ] {
        assert_eq!(strict_input_schema(&schema).expect_err(expected), expected);
    }
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

// E3: a tool_use stop and tool_use blocks come together.

#[tokio::test]
async fn a_tool_use_stop_without_calls_is_refused() {
    // Daybreak r2-repro-03.
    let body = message(
        &json!([{"type": "text", "text": "accepted mismatch"}]),
        "tool_use",
        &usage(10, 2),
    );
    assert_eq!(invalid(&body), "tool_use stop with no tool_use block");
    let server = FakeHttp::start([Reply::json(200, &body)]).await;
    let provider = AnthropicProvider::new(config_with(&base(&server), KEY)).expect("provider");
    let mut harness = harness_for(provider, short_lifecycle_config());
    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("refused");
    assert!(matches!(error, HarnessError::Provider(_)), "{error}");
}

#[test]
fn tool_blocks_without_a_tool_use_stop_are_refused() {
    let call = json!([{"type": "tool_use", "id": "toolu_1", "name": "list_dir", "input": {}}]);
    for stop in ["end_turn", "stop_sequence"] {
        assert_eq!(
            invalid(&message(&call, stop, &usage(10, 10))),
            "tool_use blocks without a tool_use stop",
            "{stop}"
        );
    }
    // A truncated or refused turn may hold blocks; the engine refuses those.
    for (stop, expected) in [
        ("max_tokens", kiln_harness::provider::StopReason::MaxTokens),
        ("refusal", kiln_harness::provider::StopReason::Refusal),
    ] {
        let parsed =
            parse_response(&message(&call, stop, &usage(10, 10)), "claude-opus-5-5").expect(stop);
        assert_eq!(parsed.stop_reason, expected);
    }
}

// The live usage shape: one `message` iteration with a null model.

fn live_body(model: &str) -> Value {
    let usage: Value = serde_json::from_str(common::anthropic::LIVE_USAGE).expect("fixture");
    let mut body = message(&json!([{"type": "text", "text": "ok"}]), "end_turn", &usage);
    body["model"] = json!(model);
    body
}

#[test]
fn the_captured_live_usage_parses() {
    let fixture: Value = serde_json::from_str(common::anthropic::LIVE_USAGE).expect("fixture");
    assert_eq!(
        common::anthropic::live_usage(18, 4, 0, 0),
        fixture,
        "the test helper is the captured shape"
    );
    let response = parse_response(&live_body("claude-opus-5-5"), "claude-opus-5-5")
        .expect("the real API's usage is valid");
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 18,
            output_tokens: 4,
            ..Usage::default()
        },
        "one iteration mirroring the top level is counted once"
    );
    // Unknown fields, now or later, at either level are ignored.
    let mut future = live_body("claude-opus-5-5");
    future["usage"]["server_tool_use"] = json!({"web_search_requests": 0});
    future["usage"]["iterations"][0]["future_field"] = json!({"x": [1, 2]});
    parse_response(&future, "claude-opus-5-5").expect("extra fields are ignored");
}

#[tokio::test]
async fn the_captured_live_usage_runs_a_session() {
    let server = FakeHttp::start([Reply::json(200, &live_body("claude-opus-5-5"))]).await;
    let temp = common::private_tempdir().expect("tempdir");
    let path = temp.path().join("live-shape.jsonl");
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
    assert_eq!(outcome.final_answer, "ok");
    assert_eq!(
        (outcome.usage.input_tokens, outcome.usage.output_tokens),
        (18, 4)
    );
    read_journal(&path).expect("verifies");
}

#[test]
fn a_null_iteration_model_means_the_requested_model() {
    // Served by another model than requested, yet the entry claims the
    // requested one: refused.
    assert_eq!(
        invalid(&live_body("claude-opus-4-8")),
        "usage iterations do not end with the serving attempt the top level reports"
    );
    // The same body is consistent for a request that named that model.
    parse_response(&live_body("claude-opus-4-8"), "claude-opus-4-8")
        .expect("null model is the requested model");
    // A fallback attempt must name its model.
    let mut body = live_body("claude-opus-5-5");
    body["usage"]["iterations"][0]["type"] = json!("fallback_message");
    assert!(invalid(&body).contains("iterations[0] has no valid model"));
    body["usage"]["iterations"][0]
        .as_object_mut()
        .expect("entry")
        .remove("model");
    assert!(invalid(&body).contains("iterations[0] has no valid model"));
    // A message attempt may also omit it; a named one must be valid.
    let mut body = live_body("claude-opus-5-5");
    body["usage"]["iterations"][0]
        .as_object_mut()
        .expect("entry")
        .remove("model");
    parse_response(&body, "claude-opus-5-5").expect("absent is null");
    body["usage"]["iterations"][0]["model"] = json!(7);
    assert!(invalid(&body).contains("iterations[0] has no valid model"));
}
