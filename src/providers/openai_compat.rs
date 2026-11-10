//! An adapter for the OpenAI-compatible `/v1/chat/completions` API.
//!
//! The MLX server (`mlx_lm.server`), Ollama, llama.cpp's server, and vLLM
//! all speak this API, so one adapter runs local models on any of them.
//!
//! # Mapping
//!
//! One [`ModelRequest`] becomes one non-streaming request:
//! `{"model", "messages", "tools", "tool_choice": "auto", "max_tokens",
//! "temperature", "seed"?, "stream": false}`. The system prompt is the first
//! message when it is not empty. An assistant turn carries its text (or
//! `null`) and its tool calls, whose arguments are sent as compact JSON
//! strings; each tool result becomes one `tool` message, in order, whose
//! content is the result's compact JSON.
//!
//! The first choice of the response becomes a [`ModelResponse`]. Its text
//! is `message.content`, with an empty string treated as no text. Each tool
//! call keeps the server's `id` when it passes the engine's call-ID rule and
//! is otherwise given `call-<turn>-<index>`. Arguments that parse to a JSON
//! object are used as that object; anything else is kept as the raw string,
//! so the typed tool rejects it and the model sees the error. Reasoning
//! side-channels such as `reasoning_content` are not part of the transcript
//! and are ignored, as are unknown fields.
//!
//! # Usage fails closed
//!
//! A response without `usage` (or without its `prompt_tokens` and
//! `completion_tokens`) is an `InvalidResponse` error, and so is usage that
//! is plainly wrong: zero prompt tokens, zero completion tokens for
//! non-empty output, or more than [`MAX_OUTPUT_BYTES_PER_TOKEN`] bytes of
//! output per completion token. Reported counts are trusted only for cost
//! accounting; the engine bounds output size in bytes on its own. Cached
//! prompt tokens (`prompt_tokens_details.cached_tokens`) are moved out of
//! `input_tokens` into `cache_read_input_tokens`, which keeps the counters
//! disjoint.
//!
//! # Credentials
//!
//! With an API key, nothing the endpoint sends is quoted: an error is its
//! status and `body withheld: authenticated request`. A success is refused
//! as `Rejected` ("response contained the request credential") when the
//! key, exactly or in its JSON-escaped spelling, appears in the raw body, in
//! any decoded JSON string or object key, or in anything kept from the
//! parsed response, including the JSON nested in `function.arguments`
//! decoded whatever its type, so neither the key nor its JSON-escaped form
//! reaches a journal. Tool names over [`MAX_TOOL_NAME_BYTES`] are refused
//! before they are kept. An
//! endpoint that holds the key can still return it transformed or split;
//! that cannot be detected.
//!
//! # Bounds, retries, and cancellation
//!
//! The serialized request is checked against `max_request_bytes` before
//! anything is sent; a larger one is a non-retryable `ContextOverflow`. The
//! response body is read chunk by chunk and abandoned at
//! `max_response_bytes`; it is never buffered unbounded. The cap counts
//! decoded HTTP payload bytes. No decompression is enabled, so that is the
//! body as sent; transfer framing (chunk headers and extensions, HTTP/2
//! frames) is not counted and is bounded only by the session deadline and
//! cancellation, and by hyper's own limits such as 16 KiB of chunk
//! extensions per message.
//!
//! Connect errors, timeouts, other transport failures, HTTP 408, 409, 429,
//! 500, 502, 503 and 504, and any other 5xx whose body is not JSON are
//! retried up to `max_attempts` times with full-jitter exponential backoff
//! capped at `retry_max_delay`. A `Retry-After` header, in seconds or as an
//! HTTP date, replaces the jittered delay and is capped the same way. The
//! jitter is a small xorshift generator seeded from the session ID and
//! turn, so a session's retry schedule is reproducible. HTTP 400 that
//! mentions the context length, and 413, are `ContextOverflow`; every other
//! 4xx (and 3xx: redirects are not followed) is `Rejected`. Without a key,
//! error messages carry the status and at most 512 bytes of the body; they
//! never carry a request header.
//!
//! The engine calls [`Provider::complete`] under the session deadline and
//! cancellation and drops the future when either fires. Dropping it aborts
//! the HTTP request, closing its connection, and any backoff sleep.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Client, StatusCode, Url};
use serde_json::{Map, Value, json};
use thiserror::Error;

use super::SecretString;
use super::http::{
    self, Credential, Failure, Transport, check_completion_tokens, excerpt, invalid,
};
pub use super::http::{
    ERROR_EXCERPT_BYTES, MAX_ATTEMPTS, MAX_BODY_BYTES, MAX_CONFIG_DELAY,
    MAX_OUTPUT_BYTES_PER_TOKEN, MAX_TOOL_NAME_BYTES,
};
use crate::engine::call_id_is_valid;
use crate::provider::{
    Message, ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind, ProviderInfo,
    StopReason, ToolCall, Usage,
};

/// The name recorded in journals for this adapter.
pub const PROVIDER_NAME: &str = "openai-compat";

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("invalid openai-compat configuration: {0}")]
pub struct OpenAiCompatError(pub String);

fn config_error(message: impl Into<String>) -> OpenAiCompatError {
    OpenAiCompatError(message.into())
}

/// Connection, sampling, bound, and retry settings for one endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    /// The API root, such as `http://192.168.1.20:11434/v1`; requests go to
    /// `<base_url>/chat/completions`.
    pub base_url: Url,
    pub model: String,
    pub api_key: Option<SecretString>,
    pub temperature: f32,
    pub seed: Option<u64>,
    pub connect_timeout: Duration,
    pub max_response_bytes: usize,
    pub max_request_bytes: usize,
    pub max_attempts: u32,
    pub retry_base_delay: Duration,
    pub retry_max_delay: Duration,
}

impl OpenAiCompatConfig {
    /// A configuration with the default bounds, sampling, and retry policy.
    ///
    /// # Errors
    ///
    /// Returns an error when `base_url` does not parse as a URL. Everything
    /// else is checked by [`OpenAiCompatConfig::validate`].
    pub fn new(base_url: &str, model: impl Into<String>) -> Result<Self, OpenAiCompatError> {
        let base_url =
            Url::parse(base_url).map_err(|error| config_error(format!("base URL: {error}")))?;
        Ok(Self {
            base_url,
            model: model.into(),
            api_key: None,
            temperature: 0.0,
            seed: None,
            connect_timeout: Duration::from_secs(10),
            max_response_bytes: 4 << 20,
            max_request_bytes: 8 << 20,
            max_attempts: 3,
            retry_base_delay: Duration::from_millis(500),
            retry_max_delay: Duration::from_secs(8),
        })
    }

    /// Check every setting.
    ///
    /// # Errors
    ///
    /// Returns an error for a base URL that is not plain `http` or `https`
    /// with a host (userinfo, a query, or a fragment are refused), a model
    /// name that is not 1..=256 printable ASCII bytes, an API key over plain
    /// `http` to anything but a loopback address, an empty or non-printable
    /// key, or a sampling, bound, or retry value out of range.
    pub fn validate(&self) -> Result<(), OpenAiCompatError> {
        let https = http::check_base_url(&self.base_url).map_err(OpenAiCompatError)?;
        http::check_model(&self.model).map_err(OpenAiCompatError)?;
        if let Some(key) = &self.api_key {
            http::check_key(key, &self.base_url, https).map_err(OpenAiCompatError)?;
        }
        if !self.temperature.is_finite() || !(0.0..=2.0).contains(&self.temperature) {
            return Err(config_error("temperature must be within 0.0..=2.0"));
        }
        self.transport().check().map_err(OpenAiCompatError)
    }

    pub(crate) fn transport(&self) -> Transport {
        Transport {
            connect_timeout: self.connect_timeout,
            max_response_bytes: self.max_response_bytes,
            max_request_bytes: self.max_request_bytes,
            max_attempts: self.max_attempts,
            retry_base_delay: self.retry_base_delay,
            retry_max_delay: self.retry_max_delay,
        }
    }

    /// The URL requests are sent to.
    #[must_use]
    pub fn endpoint(&self) -> Url {
        let mut endpoint = self.base_url.clone();
        let path = format!(
            "{}/chat/completions",
            self.base_url.path().trim_end_matches('/')
        );
        endpoint.set_path(&path);
        endpoint
    }
}

/// A provider for one OpenAI-compatible endpoint and model.
pub struct OpenAiCompatProvider {
    client: Client,
    endpoint: Url,
    authorization: Option<HeaderValue>,
    config: OpenAiCompatConfig,
}

impl std::fmt::Debug for OpenAiCompatProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiCompatProvider")
            .field("endpoint", &self.endpoint.as_str())
            .field("model", &self.config.model)
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "[redacted]"),
            )
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatProvider {
    /// Validate `config` and build the HTTP client.
    ///
    /// The client follows no redirects and uses no proxy, so requests go
    /// only to the configured endpoint, and it enables no response
    /// decompression.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration is invalid or the client
    /// cannot be built.
    pub fn new(config: OpenAiCompatConfig) -> Result<Self, OpenAiCompatError> {
        config.validate()?;
        let authorization = config
            .api_key
            .as_ref()
            .map(|key| {
                let mut value = HeaderValue::try_from(format!("Bearer {}", key.expose()))
                    .map_err(|_| config_error("API key is not a valid header value"))?;
                value.set_sensitive(true);
                Ok::<_, OpenAiCompatError>(value)
            })
            .transpose()?;
        let client = config.transport().client().map_err(OpenAiCompatError)?;
        Ok(Self {
            client,
            endpoint: config.endpoint(),
            authorization,
            config,
        })
    }

    #[must_use]
    pub fn config(&self) -> &OpenAiCompatConfig {
        &self.config
    }

    /// One HTTP exchange: send, read the bounded body, classify.
    async fn attempt(&self, body: &[u8], turn: u32) -> Result<ModelResponse, Failure> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(authorization) = &self.authorization {
            headers.insert(AUTHORIZATION, authorization.clone());
        }
        let limit = self.config.max_response_bytes;
        let exchange = http::post(&self.client, &self.endpoint, headers, body, limit).await?;
        // With a key, the endpoint holds a credential: nothing it sends is
        // quoted, and a response that contains the key is refused whole.
        let key = self.config.api_key.as_ref().map(Credential::new);
        let key = key.as_ref();
        if exchange.status.is_success() {
            let value = http::decode_success(&exchange, limit, key)?;
            let parsed = parse_response_quoting(&value, turn, key.is_none())?;
            // Tool arguments are JSON inside a JSON string: a second layer.
            if key.is_some_and(|key| key.in_response(&parsed)) {
                return Err(http::credential_echo());
            }
            return Ok(parsed);
        }
        let mut failure = classify_status(exchange.status, &exchange.body, key.is_none());
        failure.retry_after = exchange.retry_after;
        Err(failure)
    }
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: PROVIDER_NAME.to_string(),
            model: self.config.model.clone(),
        }
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let body = serde_json::to_vec(&request_body(&self.config, &request)).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::InvalidResponse,
                format!("encode request: {error}"),
            )
        })?;
        let transport = self.config.transport();
        transport.check_request(&body)?;
        let this = &*self;
        transport
            .with_retries(request.session_id, request.turn, || {
                this.attempt(&body, request.turn)
            })
            .await
    }
}

/// Classify an error status. With `quote` false (an authenticated request)
/// the body still decides the class but is never quoted.
fn classify_status(status: StatusCode, body: &[u8], quote: bool) -> Failure {
    let code = status.as_u16();
    let message = if quote {
        format!("HTTP {code}: {}", excerpt(body))
    } else {
        format!("HTTP {code} (body withheld: authenticated request)")
    };
    match code {
        408 => Failure::retryable(ProviderErrorKind::Timeout, message),
        429 => Failure::retryable(ProviderErrorKind::RateLimited, message),
        503 => Failure::retryable(ProviderErrorKind::Overloaded, message),
        409 | 500 | 502 | 504 => Failure::retryable(ProviderErrorKind::Transport, message),
        // A proxy's or crashed server's error page rather than the API's own
        // JSON error.
        500..=599 if serde_json::from_slice::<Value>(body).is_err() => {
            Failure::retryable(ProviderErrorKind::Transport, message)
        }
        500..=599 => Failure::fatal(ProviderErrorKind::Transport, message),
        413 => Failure::fatal(ProviderErrorKind::ContextOverflow, message),
        400 if mentions_context_length(body) => {
            Failure::fatal(ProviderErrorKind::ContextOverflow, message)
        }
        _ => Failure::fatal(ProviderErrorKind::Rejected, message),
    }
}

fn mentions_context_length(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_lowercase();
    text.contains("context")
        && (text.contains("length") || text.contains("too long") || text.contains("maximum"))
}

/// The request body for one completion.
#[must_use]
pub fn request_body(config: &OpenAiCompatConfig, request: &ModelRequest<'_>) -> Value {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if !request.system.is_empty() {
        messages.push(json!({"role": "system", "content": request.system}));
    }
    for message in request.messages {
        match message {
            Message::User { text } => messages.push(json!({"role": "user", "content": text})),
            // Another provider's opaque content means nothing here.
            Message::Assistant {
                text, tool_calls, ..
            } => {
                let mut entry = Map::new();
                entry.insert("role".to_string(), json!("assistant"));
                entry.insert("content".to_string(), json!(text));
                if !tool_calls.is_empty() {
                    let calls = tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.call_id,
                                "type": "function",
                                "function": {
                                    "name": call.tool,
                                    "arguments": call.arguments.to_string(),
                                },
                            })
                        })
                        .collect::<Vec<_>>();
                    entry.insert("tool_calls".to_string(), Value::Array(calls));
                }
                messages.push(Value::Object(entry));
            }
            Message::ToolResults { results } => {
                for result in results {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": result.call_id,
                        "content": result.result.to_string(),
                    }));
                }
            }
        }
    }
    let mut body = Map::new();
    body.insert("model".to_string(), json!(config.model));
    body.insert("messages".to_string(), Value::Array(messages));
    // A request without tools omits tool_choice too: some servers refuse
    // tool_choice when no tools are offered.
    if !request.tools.is_empty() {
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    },
                })
            })
            .collect::<Vec<_>>();
        body.insert("tools".to_string(), Value::Array(tools));
        body.insert("tool_choice".to_string(), json!("auto"));
    }
    body.insert("max_tokens".to_string(), json!(request.max_output_tokens));
    body.insert(
        "temperature".to_string(),
        json!(wire_temperature(config.temperature)),
    );
    if let Some(seed) = config.seed {
        body.insert("seed".to_string(), json!(seed));
    }
    body.insert("stream".to_string(), json!(false));
    Value::Object(body)
}

/// The temperature as the decimal it was written as: `0.7_f32` widened to
/// f64 directly would be sent as `0.699999988079071`.
fn wire_temperature(temperature: f32) -> f64 {
    temperature
        .to_string()
        .parse()
        .unwrap_or_else(|_| f64::from(temperature))
}

/// Map a parsed response body to the harness's response.
///
/// # Errors
///
/// Returns `InvalidResponse` when the body has no choices, no message,
/// malformed tool calls, or no usage.
pub fn parse_response(value: &Value, turn: u32) -> Result<ModelResponse, ProviderError> {
    parse_response_quoting(value, turn, true)
}

/// [`parse_response`], quoting a server-reported error only when `quote`.
fn parse_response_quoting(
    value: &Value,
    turn: u32,
    quote: bool,
) -> Result<ModelResponse, ProviderError> {
    let Some(choices) = value.get("choices").and_then(Value::as_array) else {
        if let Some(error) = value.get("error") {
            return Err(invalid(if quote {
                format!(
                    "server reported an error: {}",
                    excerpt(error.to_string().as_bytes())
                )
            } else {
                "server reported an error (body withheld: authenticated request)".to_string()
            }));
        }
        return Err(invalid("response has no choices"));
    };
    let choice = choices
        .first()
        .ok_or_else(|| invalid("response has zero choices"))?;
    let message = choice
        .get("message")
        .filter(|message| message.is_object())
        .ok_or_else(|| invalid("choice has no message"))?;
    let text = match message.get("content") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(invalid("message content is not a string or null")),
    };
    let tool_calls = match message.get("tool_calls") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(calls)) => calls
            .iter()
            .enumerate()
            .map(|(position, call)| parse_tool_call(call, turn, position))
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(invalid("message tool_calls is not an array")),
    };
    let stop_reason = match choice.get("finish_reason").and_then(Value::as_str) {
        Some("stop") if !tool_calls.is_empty() => StopReason::ToolUse,
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls" | "function_call") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        _ => StopReason::Other,
    };
    let response = ModelResponse {
        text,
        tool_calls,
        stop_reason,
        usage: parse_usage(value.get("usage"))?,
        provider_content: None,
    };
    check_completion_tokens(&response)?;
    Ok(response)
}

fn parse_tool_call(call: &Value, turn: u32, position: usize) -> Result<ToolCall, ProviderError> {
    let function = call
        .get("function")
        .filter(|function| function.is_object())
        .ok_or_else(|| invalid("tool call has no function"))?;
    let tool = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("tool call has no function name"))?;
    // Checked before the name is copied anywhere. A name within the bound
    // that is not registered still becomes a model-visible unknown_tool.
    if tool.len() > MAX_TOOL_NAME_BYTES {
        return Err(invalid(format!(
            "tool name exceeds {MAX_TOOL_NAME_BYTES} bytes"
        )));
    }
    // A server ID is copied only once it passes the engine's rule (at most
    // 64 bytes); otherwise a short positional ID replaces it.
    let call_id = match call.get("id").and_then(Value::as_str) {
        Some(id) if call_id_is_valid(id) => id.to_string(),
        _ => format!("call-{turn}-{}", position + 1),
    };
    let arguments = match function.get("arguments") {
        Some(Value::String(raw)) => match serde_json::from_str::<Value>(raw) {
            Ok(parsed @ Value::Object(_)) => parsed,
            _ => Value::String(raw.clone()),
        },
        Some(other) => other.clone(),
        None => Value::Null,
    };
    Ok(ToolCall {
        call_id,
        tool: tool.to_string(),
        arguments,
    })
}

fn parse_usage(usage: Option<&Value>) -> Result<Usage, ProviderError> {
    let usage = usage
        .filter(|usage| usage.is_object())
        .ok_or_else(|| invalid("response has no usage; token budgets cannot be enforced"))?;
    let count = |field: &str| {
        usage
            .get(field)
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid(format!("usage has no {field}")))
    };
    let prompt = count("prompt_tokens")?;
    // Every request carries at least the user's task.
    if prompt == 0 {
        return Err(invalid("usage reports zero prompt tokens"));
    }
    let completion = count("completion_tokens")?;
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok(Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: completion,
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> OpenAiCompatConfig {
        OpenAiCompatConfig::new("http://127.0.0.1:8080/v1", "test-model").expect("config")
    }

    #[test]
    fn endpoint_joins_the_base_path() {
        assert_eq!(
            config().endpoint().as_str(),
            "http://127.0.0.1:8080/v1/chat/completions"
        );
        let mut slash = config();
        slash.base_url = Url::parse("http://h:1/v1/").expect("url");
        assert_eq!(slash.endpoint().as_str(), "http://h:1/v1/chat/completions");
    }

    #[test]
    fn keys_need_https_except_on_loopback() {
        let mut lan = OpenAiCompatConfig::new("http://192.168.1.20:11434/v1", "m").expect("url");
        lan.validate()
            .expect("plain http to a LAN server without a key");
        lan.api_key = Some(SecretString::new("sk-test"));
        assert!(lan.validate().is_err());
        for loopback in ["http://127.0.0.1:1/v1", "http://[::1]:1/v1"] {
            let mut config = OpenAiCompatConfig::new(loopback, "m").expect("url");
            config.api_key = Some(SecretString::new("sk-test"));
            config.validate().expect("loopback");
        }
        let mut named = OpenAiCompatConfig::new("http://localhost:1/v1", "m").expect("url");
        named.api_key = Some(SecretString::new("sk-test"));
        assert!(
            named.validate().is_err(),
            "names are not trusted as loopback"
        );
        let mut https = OpenAiCompatConfig::new("https://api.example.com/v1", "m").expect("url");
        https.api_key = Some(SecretString::new("sk-test"));
        https.validate().expect("https");
        https.api_key = Some(SecretString::new("sk test"));
        let error = https.validate().expect_err("space in key");
        assert!(!error.to_string().contains("sk test"));
    }

    #[test]
    fn statuses_are_classified() {
        let cases: &[(u16, &[u8], ProviderErrorKind, bool)] = &[
            (408, b"", ProviderErrorKind::Timeout, true),
            (409, b"", ProviderErrorKind::Transport, true),
            (429, b"", ProviderErrorKind::RateLimited, true),
            (500, b"{}", ProviderErrorKind::Transport, true),
            (502, b"", ProviderErrorKind::Transport, true),
            (503, b"", ProviderErrorKind::Overloaded, true),
            (504, b"", ProviderErrorKind::Transport, true),
            (501, b"<html>", ProviderErrorKind::Transport, true),
            (
                501,
                b"{\"error\":\"no\"}",
                ProviderErrorKind::Transport,
                false,
            ),
            (
                400,
                b"{\"error\":{\"message\":\"This model's maximum context length is 8192 tokens\"}}",
                ProviderErrorKind::ContextOverflow,
                false,
            ),
            (
                400,
                b"prompt exceeds Context: too long",
                ProviderErrorKind::ContextOverflow,
                false,
            ),
            (400, b"bad request", ProviderErrorKind::Rejected, false),
            (401, b"", ProviderErrorKind::Rejected, false),
            (404, b"", ProviderErrorKind::Rejected, false),
            (413, b"", ProviderErrorKind::ContextOverflow, false),
            (302, b"", ProviderErrorKind::Rejected, false),
        ];
        for (code, body, kind, retryable) in cases {
            let status = StatusCode::from_u16(*code).expect("status");
            let failure = classify_status(status, body, true);
            let withheld = classify_status(status, body, false);
            assert_eq!(withheld.error.kind, *kind, "{code}");
            assert_eq!(withheld.error.retryable, *retryable, "{code}");
            assert_eq!(
                withheld.error.message,
                format!("HTTP {code} (body withheld: authenticated request)")
            );
            assert_eq!(failure.error.kind, *kind, "{code}");
            assert_eq!(failure.error.retryable, *retryable, "{code}");
            assert!(failure.error.message.starts_with(&format!("HTTP {code}")));
        }
    }
}
