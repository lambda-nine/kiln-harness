//! Regression tests for the first adversarial review of the
//! openai-compat provider: credential echoes, implausible usage, and
//! transfer framing.
mod common;

use std::time::{Duration, Instant};

use common::http::{FakeHttp, Reply};
use common::{FakeApi, short_lifecycle_config};
use kiln_harness::audit::JsonlAudit;
use kiln_harness::engine::{Harness, HarnessError, SessionTask};
use kiln_harness::journal::{JournalEvent, JsonlJournal, MemoryJournal, read_journal};
use kiln_harness::provider::{
    Message, ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind,
};
use kiln_harness::providers::{OpenAiCompatConfig, OpenAiCompatProvider, SecretString};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const SIMPLE_KEY: &str = "sk-DB-SUCCESS-RESPONSE-8f317";
/// Contains `"` and `\`, so its JSON-escaped form differs from it.
const ESCAPED_KEY: &str = r#"sk-DB-\"quote\\slash-482c"#;
const CREDENTIAL_ECHO: &str = "response contained the request credential";
const WITHHELD_401: &str = "HTTP 401 (body withheld: authenticated request)";

fn config(base_url: &str, key: Option<&str>) -> OpenAiCompatConfig {
    let mut config = OpenAiCompatConfig::new(base_url, "test-model").expect("config");
    config.retry_base_delay = Duration::from_millis(1);
    config.retry_max_delay = Duration::from_millis(1);
    config.api_key = key.map(SecretString::new);
    config
}

fn completion(message: &Value, usage: &Value) -> Value {
    json!({
        "choices": [{"message": message, "finish_reason": "stop"}],
        "usage": usage,
    })
}

fn text(content: &str) -> Value {
    completion(
        &json!({"role": "assistant", "content": content}),
        &json!({"prompt_tokens": 10, "completion_tokens": 10}),
    )
}

fn tool_call(arguments: &Value) -> Value {
    completion(
        &json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_1", "type": "function",
             "function": {"name": "list_dir", "arguments": arguments}}
        ]}),
        &json!({"prompt_tokens": 10, "completion_tokens": 10}),
    )
}

async fn complete(server: &FakeHttp, key: Option<&str>) -> Result<ModelResponse, ProviderError> {
    complete_with(config(&server.base_url(), key)).await
}

async fn complete_with(config: OpenAiCompatConfig) -> Result<ModelResponse, ProviderError> {
    let mut provider = OpenAiCompatProvider::new(config).expect("provider");
    let messages = [Message::User {
        text: "hi".to_string(),
    }];
    provider
        .complete(ModelRequest {
            session_id: "hs-boundaries",
            turn: 1,
            system: "",
            tools: &[],
            messages: &messages,
            max_output_tokens: 8,
        })
        .await
}

fn task() -> SessionTask {
    SessionTask {
        system: "Be brief.".to_string(),
        prompt: "Answer.".to_string(),
    }
}

fn assert_credential_echo(result: Result<ModelResponse, ProviderError>) {
    let error = result.expect_err("a response with the key is refused");
    assert_eq!(error.kind, ProviderErrorKind::Rejected);
    assert!(!error.retryable);
    assert_eq!(error.message, CREDENTIAL_ECHO);
}

// F1: the key never reaches the journal, audit, or errors.

#[tokio::test]
async fn a_success_that_echoes_the_key_is_refused_before_the_journal() {
    let temp = common::private_tempdir().expect("tempdir");
    let journal_path = temp.path().join("run.jsonl");
    let audit_path = temp.path().join("audit.jsonl");
    let server = FakeHttp::start([Reply::json(200, &text(SIMPLE_KEY))]).await;
    let provider =
        OpenAiCompatProvider::new(config(&server.base_url(), Some(SIMPLE_KEY))).expect("provider");
    let api = FakeApi::default();
    let mut harness = Harness::new(
        api.clone(),
        provider,
        JsonlAudit::open(&audit_path).expect("audit"),
        JsonlJournal::open(&journal_path).expect("journal"),
        short_lifecycle_config(),
    );

    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("refused");

    assert!(
        matches!(&error, HarnessError::Provider(error) if error.message == CREDENTIAL_ECHO),
        "{error}"
    );
    for path in [&journal_path, &audit_path] {
        let text = std::fs::read_to_string(path).expect("read");
        assert!(!text.contains(SIMPLE_KEY), "{} has the key", path.display());
    }
    let journal = read_journal(&journal_path).expect("journal verifies");
    assert!(journal.records.iter().any(|record| matches!(
        &record.event,
        JournalEvent::ModelFailed { message, .. } if message == CREDENTIAL_ECHO
    )));
    assert_eq!(api.state().deleted.len(), 1);
}

#[tokio::test]
async fn a_json_escaped_echo_is_caught_in_success_and_error_bodies() {
    // The raw body holds only the escaped form; the decoded string holds
    // the key itself.
    let server = FakeHttp::start([Reply::json(200, &text(&format!("key: {ESCAPED_KEY}")))]).await;
    let raw = text(ESCAPED_KEY).to_string();
    assert!(
        !raw.contains(ESCAPED_KEY),
        "the fixture must escape the key"
    );
    assert_credential_echo(complete(&server, Some(ESCAPED_KEY)).await);

    let body = json!({"error": {"message": format!("invalid key {ESCAPED_KEY}")}});
    let server = FakeHttp::start([Reply::json(401, &body)]).await;
    let error = complete(&server, Some(ESCAPED_KEY)).await.expect_err("401");
    assert_eq!(error.kind, ProviderErrorKind::Rejected);
    assert_eq!(error.message, WITHHELD_401);
}

#[tokio::test]
async fn a_key_across_the_excerpt_boundary_is_not_quoted() {
    let body = format!("{}{SIMPLE_KEY}", "x".repeat(490));
    let server = FakeHttp::start([Reply::status(401, &body)]).await;
    let error = complete(&server, Some(SIMPLE_KEY)).await.expect_err("401");
    assert_eq!(error.message, WITHHELD_401);
    assert!(!error.message.contains(&SIMPLE_KEY[..22]));

    // Unauthenticated local servers keep their excerpts.
    let server = FakeHttp::start([Reply::status(401, "no such model")]).await;
    let error = complete(&server, None).await.expect_err("401");
    assert_eq!(error.message, "HTTP 401: no such model");
}

#[tokio::test]
async fn an_authenticated_error_reported_in_a_success_body_is_not_quoted() {
    let server = FakeHttp::start([Reply::json(
        200,
        &json!({"error": {"message": "quota exceeded for this key"}}),
    )])
    .await;
    let error = complete(&server, Some(SIMPLE_KEY))
        .await
        .expect_err("no choices");
    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
    assert!(!error.message.contains("quota"), "{error}");
    assert!(error.message.contains("body withheld"), "{error}");
}

#[tokio::test]
async fn a_key_inside_tool_arguments_is_refused() {
    // In a string-encoded argument the key is escaped twice on the wire,
    // and appears whole only after the arguments are parsed.
    let arguments = json!({"path": ESCAPED_KEY}).to_string();
    let server = FakeHttp::start([Reply::json(200, &tool_call(&json!(arguments)))]).await;
    assert_credential_echo(complete(&server, Some(ESCAPED_KEY)).await);

    // As an object key of object-encoded arguments.
    let mut object = serde_json::Map::new();
    object.insert(ESCAPED_KEY.to_string(), json!(1));
    let server = FakeHttp::start([Reply::json(200, &tool_call(&Value::Object(object)))]).await;
    assert_credential_echo(complete(&server, Some(ESCAPED_KEY)).await);

    // In plain view: in a call ID, or as the tool name.
    let mut echoed = tool_call(&json!("{}"));
    echoed["choices"][0]["message"]["tool_calls"][0]["function"]["name"] = json!(SIMPLE_KEY);
    let server = FakeHttp::start([Reply::json(200, &echoed)]).await;
    assert_credential_echo(complete(&server, Some(SIMPLE_KEY)).await);

    // The same responses are fine without a key to protect.
    let arguments = json!({"path": ESCAPED_KEY}).to_string();
    let server = FakeHttp::start([Reply::json(200, &tool_call(&json!(arguments)))]).await;
    let response = complete(&server, None).await.expect("no credential");
    assert_eq!(response.tool_calls[0].arguments["path"], ESCAPED_KEY);
}

// F2: implausible usage is refused, and output size is bounded locally.

#[tokio::test]
async fn zero_usage_cannot_carry_a_large_answer_past_the_budgets() {
    let body = completion(
        &json!({"role": "assistant", "content": "x".repeat(60_000)}),
        &json!({"prompt_tokens": 0, "completion_tokens": 0}),
    );
    let server = FakeHttp::start([Reply::json(200, &body)]).await;
    let provider = OpenAiCompatProvider::new(config(&server.base_url(), None)).expect("provider");
    let mut limits = short_lifecycle_config();
    limits.max_output_tokens_per_request = 1;
    limits.max_total_input_tokens = 1;
    limits.max_total_output_tokens = 1;
    let mut harness = Harness::new(
        FakeApi::default(),
        provider,
        kiln_harness::audit::MemoryAudit::default(),
        MemoryJournal::default(),
        limits,
    );

    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("zero usage is refused");

    assert!(
        matches!(&error, HarnessError::Provider(error)
            if error.kind == ProviderErrorKind::InvalidResponse),
        "{error}"
    );
}

#[tokio::test]
async fn zero_or_grossly_low_counts_are_invalid() {
    let cases = [
        (
            "x",
            json!({"prompt_tokens": 0, "completion_tokens": 1}),
            "zero prompt tokens",
        ),
        (
            "x",
            json!({"prompt_tokens": 5, "completion_tokens": 0}),
            "zero completion tokens",
        ),
        (
            &"y".repeat(65) as &str,
            json!({"prompt_tokens": 5, "completion_tokens": 1}),
            "64 bytes per token",
        ),
        (
            &"z".repeat(6401),
            json!({"prompt_tokens": 5, "completion_tokens": 100}),
            "100 completion tokens for 6401 bytes",
        ),
    ];
    for (content, usage, needle) in cases {
        let body = completion(&json!({"role": "assistant", "content": content}), &usage);
        let server = FakeHttp::start([Reply::json(200, &body)]).await;
        let error = complete(&server, None).await.expect_err(needle);
        assert_eq!(error.kind, ProviderErrorKind::InvalidResponse, "{needle}");
        assert!(error.message.contains(needle), "{error}");
        assert!(!error.retryable);
    }
}

#[tokio::test]
async fn counts_at_the_floor_are_accepted() {
    // 64 bytes for one token, exactly at the floor.
    let body = completion(
        &json!({"role": "assistant", "content": "w".repeat(64)}),
        &json!({"prompt_tokens": 5, "completion_tokens": 1}),
    );
    let server = FakeHttp::start([Reply::json(200, &body)]).await;
    let response = complete(&server, None).await.expect("at the floor");
    assert_eq!(response.output_bytes(), 64);

    // Tool-call arguments count as compact JSON: {"path":"."} is 12 bytes.
    let body = completion(
        &json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "c", "function": {"name": "list_dir", "arguments": "{ \"path\" : \".\" }"}}
        ]}),
        &json!({"prompt_tokens": 5, "completion_tokens": 1}),
    );
    let server = FakeHttp::start([Reply::json(200, &body)]).await;
    let response = complete(&server, None).await.expect("small call");
    assert_eq!(response.output_bytes(), 12);
}

// F3: the response cap counts decoded payload bytes; framing is bounded by
// the deadline.

#[tokio::test]
async fn chunk_framing_is_not_payload_but_the_payload_cap_still_holds() {
    let payload = text("ok").to_string().into_bytes();
    assert!(payload.len() < 256);
    let server = FakeHttp::start([Reply::Framed {
        body: payload.clone(),
        extension: 64,
        interval: Duration::ZERO,
        endless: false,
    }])
    .await;
    let mut accepted = config(&server.base_url(), None);
    accepted.max_response_bytes = 256;
    let response = complete_with(accepted)
        .await
        .expect("payload under the cap");
    assert_eq!(response.text.as_deref(), Some("ok"));
    // The framing was many times the cap; only the payload is counted.
    assert!(server.state().streamed_bytes > 256 * 20);

    let mut padded = payload.clone();
    padded.extend(std::iter::repeat_n(b' ', 300));
    let server = FakeHttp::start([Reply::Framed {
        body: padded,
        // Under hyper's 16 KiB limit on a message's chunk extensions.
        extension: 16,
        interval: Duration::ZERO,
        endless: false,
    }])
    .await;
    let mut capped = config(&server.base_url(), None);
    capped.max_response_bytes = 256;
    let error = complete_with(capped)
        .await
        .expect_err("payload over the cap");
    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
    assert_eq!(error.message, "response exceeds 256 bytes");

    // hyper itself refuses more than 16 KiB of chunk extensions in one
    // message, as a transport error.
    let server = FakeHttp::start([Reply::Framed {
        body: payload,
        extension: 200,
        interval: Duration::ZERO,
        endless: false,
    }])
    .await;
    let mut single = config(&server.base_url(), None);
    single.max_attempts = 1;
    let error = complete_with(single)
        .await
        .expect_err("extensions over 16 KiB");
    assert_eq!(error.kind, ProviderErrorKind::Transport, "{error}");
}

#[tokio::test]
async fn a_slow_framing_server_is_ended_by_the_session_deadline() {
    let server = FakeHttp::start([Reply::Framed {
        body: Vec::new(),
        extension: 64,
        interval: Duration::from_millis(5),
        endless: true,
    }])
    .await;
    let provider = OpenAiCompatProvider::new(config(&server.base_url(), None)).expect("provider");
    let api = FakeApi::default();
    let mut limits = short_lifecycle_config();
    limits.session_timeout = Duration::from_millis(500);
    let mut harness = Harness::new(
        api.clone(),
        provider,
        kiln_harness::audit::MemoryAudit::default(),
        MemoryJournal::default(),
        limits,
    );
    let started = Instant::now();

    let error = harness
        .run(&task(), CancellationToken::new())
        .await
        .expect_err("deadline");

    assert!(matches!(error, HarnessError::Deadline), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
    server
        .wait_for(Duration::from_secs(3), |state| state.streams_ended == 1)
        .await;
    assert_eq!(api.state().deleted.len(), 1);
}
