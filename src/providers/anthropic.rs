//! An adapter for Anthropic's Messages API (`POST /v1/messages`).
//!
//! There is no official Rust SDK, so this speaks the documented wire format
//! over the shared HTTP layer in [`super::http`], API version
//! [`API_VERSION`].
//!
//! # Request
//!
//! One [`ModelRequest`] becomes one non-streaming request:
//! `{"model", "max_tokens", "system"?, "messages", "tools"?, "tool_choice"?,
//! "output_config": {"effort"}, "cache_control": {"type": "ephemeral"},
//! "fallbacks"?}` with the `x-api-key` (sensitive) and `anthropic-version`
//! headers. `temperature`, `top_p`, `top_k`, `thinking`, and `stream` are
//! never sent: sampling parameters are refused on this API version, and
//! without `thinking` current models think adaptively.
//!
//! - A user turn is `{"role": "user", "content": text}`.
//! - An assistant turn this adapter produced is sent back as its recorded
//!   [`ProviderContent`] blocks, unchanged and in their original order.
//!   Preserved thinking validates each `thinking` block's `signature` and
//!   the position of each `fallback` block; editing, reordering, or dropping
//!   any block breaks the turn. The blocks are held as JSON values, so the
//!   echo is value-for-value: strings and numbers are exact, and object keys
//!   are written in the journal's canonical (sorted) order.
//! - An assistant turn from another provider, which only a transcript
//!   recorded elsewhere can contain, is rebuilt as a text block (if any)
//!   followed by `tool_use` blocks whose `input` is the arguments object, or
//!   `{}` when the sanitized arguments are not an object.
//! - One turn's tool results are one user message of `tool_result` blocks,
//!   in order: the result's compact JSON as `content`, and `is_error` unless
//!   the result says `"ok": true`.
//!
//! Tools come first, then the system prompt, then the messages, all
//! serialized deterministically, so each request's rendered prompt starts
//! with the previous one's and top-level `cache_control` (automatic prompt
//! caching) reuses it. Nothing in the request is reordered between turns.
//!
//! # Response
//!
//! The body must be the non-streaming success envelope: `type: "message"`,
//! `role: "assistant"`, a `model` that is 1..=256 bytes of letters, digits,
//! and `._:@/-`, a `content` array, a known `stop_reason`, and `usage`. A
//! `type: "error"` body is an error whatever else it carries.
//!
//! `content` is kept whole as [`ProviderContent::blocks`], with the
//! response's `model` (the model that actually served the turn) and any
//! `stop_details` (recorded, never sent back). From it, `text` blocks are
//! joined with `"\n"` into the neutral text and `tool_use` blocks become
//! [`ToolCall`]s, in order; every other block type (`thinking`,
//! `redacted_thinking`, `fallback`, and anything unknown) lives only in the
//! opaque content. A `tool_use` must have an ID that passes the engine's
//! call-ID rule, a name of at most [`MAX_TOOL_NAME_BYTES`], and an object
//! `input`, or the response is `InvalidResponse`: the opaque echo carries the
//! original ID, so it cannot be replaced the way the openai-compat adapter
//! replaces one.
//!
//! | `stop_reason` | [`StopReason`] |
//! |---|---|
//! | `end_turn`, `stop_sequence` | `EndTurn`; `InvalidResponse` with `tool_use` blocks |
//! | `tool_use` | `ToolUse`; `InvalidResponse` without `tool_use` blocks |
//! | `max_tokens`, `model_context_window_exceeded` | `MaxTokens` |
//! | `refusal` | `Refusal` |
//! | `pause_turn` | `InvalidResponse`: it needs server tools, never sent |
//! | missing or anything else | `InvalidResponse` |
//!
//! The adapter never produces [`StopReason::Other`], so nothing it does not
//! understand ends a session as a final answer. With client tools only, a
//! `tool_use` stop and `tool_use` blocks come together, and a mismatch is
//! refused rather than normalized; a `max_tokens` or `refusal` turn may
//! still hold blocks, and the engine refuses it as truncated or refused.
//!
//! Opaque content must echo value for value, and `serde_json` holds only
//! 64-bit integers exactly: a float, or an integer outside `i64` and `u64`,
//! anywhere in the blocks or `stop_details`, `tool_use` inputs included, is
//! `InvalidResponse`: a `tool_use` block is echoed from the blocks like
//! any other, so its input is on the same verbatim path. Current block
//! types (`thinking`, `redacted_thinking`, `fallback`, `text`) carry no such
//! numbers, and every registered tool takes only strings, string arrays,
//! and integers, so a float argument would fail typed validation anyway;
//! this costs nothing in practice.
//!
//! `usage` is required. Its `input_tokens`, `cache_read_input_tokens`, and
//! `cache_creation_input_tokens` are already disjoint, as the provider
//! contract wants; a missing or null counter is 0. The top-level counters
//! are always checked; they describe the attempt that produced the returned
//! message. With a server-side fallback, `usage.iterations` records every
//! attempt: each entry must have a known `type` ([`ITERATION_TYPES`]), a
//! valid `model`, valid counters, and nonzero input, and the last must be
//! the serving attempt, with the response's `model` and exactly the
//! top-level counters, or the response is `InvalidResponse`. The usage is
//! then the saturating sum of every entry, without the top level counted
//! again, so the per-request output cap covers one model call with all its
//! attempts; `--no-fallbacks` gives a strict per-attempt cap. Zero total
//! input (overall and for the serving attempt), and zero output tokens for
//! non-empty content or more than
//! [`MAX_OUTPUT_BYTES_PER_TOKEN`](super::http::MAX_OUTPUT_BYTES_PER_TOKEN)
//! bytes of text and calls per output token, judged on the serving
//! attempt's output since it produced the content, are `InvalidResponse`. Opaque blocks count toward the engine's byte budget
//! but not the token floor: a thinking signature is large and unrelated to
//! completion tokens.
//!
//! # Decisions
//!
//! - **Fallbacks on by default.** With `fallbacks` set the request carries
//!   `fallbacks: "default"` and the `server-side-fallback-2026-07-01` beta.
//!   On a safety decline the API reruns the request on a suitable model in
//!   the same call and returns `fallback` blocks and the serving `model`;
//!   both are stored and echoed verbatim, and the serving model is recorded
//!   as `served_model` in the audit. A non-streaming response omits a
//!   declined partial, so no block before a `fallback` block needs dropping.
//! - **Strict tools off by default.** Strict mode refuses `maxItems` and
//!   similar bounds, which the harness's schemas use. With `strict_tools`,
//!   each schema is sent through [`strict_input_schema`]: supported keywords
//!   kept, `minItems` of 0 or 1 kept, other size and range bounds removed
//!   (the typed tools enforce them), `"required": []` added where absent,
//!   and anything else (`pattern`, `multipleOf`, `$ref`, ...) refused with
//!   its path rather than silently dropped. The canonical schemas stay in
//!   [`crate::provider::ToolDefinition`].
//! - **Non-streaming only.** `run`'s 16,384-token default suits it.
//! - **No reordering**, for a stable prompt-cache prefix.
//!
//! # Errors and credentials
//!
//! With a key, nothing the endpoint sends is quoted. An error is its status,
//! its `error.type` from a closed list of known types (anything else is
//! `unknown`), and a `request_id` that is `req_` plus at most 60 ASCII
//! letters and digits, each only when it does not contain the key in either
//! spelling (otherwise the message is the bare status); without a key, the same followed by at most
//! [`ERROR_EXCERPT_BYTES`](super::http::ERROR_EXCERPT_BYTES) of the body,
//! UTF-8-lossy and without control characters. Connect errors,
//! timeouts, and HTTP 408, 409, 429, 500, 502, 503, 504, and 529 are retried
//! (as are other 5xx without a JSON body), honoring a capped `retry-after`;
//! 413, and 400 whose message is about the prompt's length or the context
//! window, are `ContextOverflow`; every other status is `Rejected`. A
//! success containing the key, exactly or JSON-escaped, anywhere in the raw
//! body or a decoded string, opaque blocks included, is refused as
//! `Rejected`. The key goes only into the `x-api-key` header, over `https`
//! unless the host is a loopback IP literal, and must be at least
//! [`MIN_API_KEY_BYTES`] long. Every error is finally passed through
//! `scrub`: if the finished message contains the key in either spelling,
//! whether from an endpoint field or by colliding with the adapter's own
//! wording, the message becomes empty and the kind is kept. Empty is the
//! only replacement no accepted key can occur in.
//!
//! # Credential-proxy mode
//!
//! Without a key the adapter sends no `x-api-key` (and no `Authorization`)
//! header at all: a credential-injecting proxy on the host adds the key, so
//! the harness, and the guest it runs in, never hold it. The proxy then
//! owns the credential scan, since the harness has no key to look for, and
//! errors are quoted under the unauthenticated rules above. Plain `http` is
//! allowed to any host because no secret crosses that hop, but prompts and
//! tool results do, so the proxy belongs on a loopback or host-only route.
//! A keyless configuration aimed at `api.anthropic.com` itself is refused:
//! it could only fail with an authentication error.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, StatusCode, Url};
use serde_json::{Map, Value, json};
use thiserror::Error;

use super::SecretString;
use super::http::{
    self, Credential, Failure, MAX_TOOL_NAME_BYTES, Transport, check_completion_tokens, excerpt,
    invalid,
};
use crate::engine::call_id_is_valid;
use crate::provider::{
    Message, ModelRequest, ModelResponse, Provider, ProviderContent, ProviderError,
    ProviderErrorKind, ProviderInfo, StopReason, ToolCall, Usage,
};
use crate::tools::{ToolLimits, ToolRegistry};

/// The name recorded in journals and in [`ProviderContent::provider`].
pub const PROVIDER_NAME: &str = "anthropic";
/// The `anthropic-version` header.
pub const API_VERSION: &str = "2026-10-21";
/// The beta that enables `fallbacks: "default"`.
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// Documented `usage.iterations` entry types: a requested-model attempt and
/// a server-side fallback attempt.
pub const ITERATION_TYPES: [&str; 2] = ["message", "fallback_message"];
/// Error types quoted in error messages; anything else is `unknown`.
pub const KNOWN_ERROR_TYPES: [&str; 10] = [
    "invalid_request_error",
    "authentication_error",
    "billing_error",
    "permission_error",
    "not_found_error",
    "request_too_large",
    "rate_limit_error",
    "api_error",
    "timeout_error",
    "overloaded_error",
];
const MAX_REQUEST_ID_BYTES: usize = 64;
/// Shortest API key accepted.
pub const MIN_API_KEY_BYTES: usize = 20;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("invalid anthropic configuration: {0}")]
pub struct AnthropicError(pub String);

/// `output_config.effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Effort {
    Low,
    Medium,
    #[default]
    High,
    Xhigh,
    Max,
}

impl Effort {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => Err("effort must be low, medium, high, xhigh, or max".to_string()),
        }
    }
}

/// Endpoint, model, key, and the same bounds and retry policy as the
/// openai-compat adapter.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// The API root, such as `https://api.anthropic.com`; requests go to
    /// `<base_url>/v1/messages`.
    pub base_url: Url,
    pub model: String,
    /// `None` for credential-proxy mode: no key header is sent.
    pub api_key: Option<SecretString>,
    pub effort: Effort,
    /// Send `fallbacks: "default"` and its beta header.
    pub fallbacks: bool,
    /// Mark every tool `"strict": true`.
    pub strict_tools: bool,
    pub connect_timeout: Duration,
    pub max_response_bytes: usize,
    pub max_request_bytes: usize,
    pub max_attempts: u32,
    pub retry_base_delay: Duration,
    pub retry_max_delay: Duration,
}

impl AnthropicConfig {
    /// A configuration with the default effort, fallbacks, bounds, and
    /// retry policy.
    ///
    /// # Errors
    ///
    /// Returns an error when `base_url` does not parse as a URL. Everything
    /// else is checked by [`AnthropicConfig::validate`].
    pub fn new(
        base_url: &str,
        model: impl Into<String>,
        api_key: Option<SecretString>,
    ) -> Result<Self, AnthropicError> {
        let base_url =
            Url::parse(base_url).map_err(|error| AnthropicError(format!("base URL: {error}")))?;
        Ok(Self {
            base_url,
            model: model.into(),
            api_key,
            effort: Effort::default(),
            fallbacks: true,
            strict_tools: false,
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
    /// with a host, a key over plain `http` to anything but a loopback IP
    /// literal, no key with `api.anthropic.com` as the host, a model name
    /// that is not 1..=256 printable ASCII bytes, an empty or non-printable
    /// key, or a bound or retry value out of range.
    pub fn validate(&self) -> Result<(), AnthropicError> {
        let https = http::check_base_url(&self.base_url).map_err(AnthropicError)?;
        http::check_model(&self.model).map_err(AnthropicError)?;
        match &self.api_key {
            Some(key) => {
                http::check_key(key, &self.base_url, https).map_err(AnthropicError)?;
                // Real keys are far longer; a short one is an operator
                // mistake and could collide with ordinary words.
                if key.expose().len() < MIN_API_KEY_BYTES {
                    return Err(AnthropicError(format!(
                        "API key looks truncated: under {MIN_API_KEY_BYTES} bytes"
                    )));
                }
            }
            None if self.targets_the_api() => {
                return Err(AnthropicError(
                    "without an API key the base URL must be a credential-injecting proxy, \
                     not the Anthropic API itself"
                        .to_string(),
                ));
            }
            None => {}
        }
        self.transport().check().map_err(AnthropicError)
    }

    /// Whether the base URL's host is Anthropic's own API host, where a
    /// keyless request can only fail.
    #[must_use]
    pub fn targets_the_api(&self) -> bool {
        self.base_url.host_str().is_some_and(|host| {
            host.trim_end_matches('.')
                .eq_ignore_ascii_case("api.anthropic.com")
        })
    }

    fn transport(&self) -> Transport {
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
        let path = format!("{}/v1/messages", self.base_url.path().trim_end_matches('/'));
        endpoint.set_path(&path);
        endpoint
    }
}

/// A provider for one Anthropic endpoint and model.
pub struct AnthropicProvider {
    client: Client,
    endpoint: Url,
    headers: HeaderMap,
    config: AnthropicConfig,
}

impl fmt::Debug for AnthropicProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicProvider")
            .field("endpoint", &self.endpoint.as_str())
            .field("model", &self.config.model)
            .field(
                "x-api-key",
                &if self.config.api_key.is_some() {
                    "[redacted]"
                } else {
                    "none: credential proxy"
                },
            )
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    /// Validate `config` and build the HTTP client, which follows no
    /// redirects, uses no proxy, and decompresses nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration is invalid or the client
    /// cannot be built.
    pub fn new(config: AnthropicConfig) -> Result<Self, AnthropicError> {
        config.validate()?;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        // Credential-proxy mode sends no key header at all.
        if let Some(api_key) = &config.api_key {
            let mut key = HeaderValue::try_from(api_key.expose())
                .map_err(|_| AnthropicError("API key is not a valid header value".to_string()))?;
            key.set_sensitive(true);
            headers.insert(HeaderName::from_static("x-api-key"), key);
        }
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static(API_VERSION),
        );
        if config.fallbacks {
            headers.insert(
                HeaderName::from_static("anthropic-beta"),
                HeaderValue::from_static(FALLBACK_BETA),
            );
        }
        if config.strict_tools {
            // The harness's own tools must survive the strict transform, so
            // --strict-tools fails here rather than mid-session.
            for tool in ToolRegistry::new(ToolLimits::default()).definitions() {
                strict_input_schema(&tool.input_schema)
                    .map_err(|problem| AnthropicError(format!("tool {}: {problem}", tool.name)))?;
            }
        }
        let client = config.transport().client().map_err(AnthropicError)?;
        Ok(Self {
            client,
            endpoint: config.endpoint(),
            headers,
            config,
        })
    }

    #[must_use]
    pub fn config(&self) -> &AnthropicConfig {
        &self.config
    }

    /// One HTTP exchange: send, read the bounded body, classify.
    async fn attempt(&self, body: &[u8]) -> Result<ModelResponse, Failure> {
        let limit = self.config.max_response_bytes;
        let exchange = http::post(
            &self.client,
            &self.endpoint,
            self.headers.clone(),
            body,
            limit,
        )
        .await?;
        // With a key, the endpoint holds a credential: nothing it sends is
        // quoted, and a response that contains the key is refused whole.
        // Without one, a proxy holds the key and owns that scan.
        let key = self.config.api_key.as_ref().map(Credential::new);
        let key = key.as_ref();
        if exchange.status.is_success() {
            let value = http::decode_success(&exchange, limit, key)?;
            let parsed = parse_response_quoting(&value, key.is_none())?;
            if key.is_some_and(|key| key.in_response(&parsed)) {
                return Err(http::credential_echo());
            }
            return Ok(parsed);
        }
        let mut failure = classify_status(exchange.status, &exchange.body, key);
        failure.retry_after = exchange.retry_after;
        Err(failure)
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
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
        // Every error this adapter returns passes here once, fully formatted.
        let result = self.complete_unscrubbed(request).await;
        result.map_err(|error| scrub(error, self.config.api_key.as_ref()))
    }
}

impl AnthropicProvider {
    async fn complete_unscrubbed(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let body = serde_json::to_vec(&request_body(&self.config, &request)?).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::InvalidResponse,
                format!("encode request: {error}"),
            )
        })?;
        let transport = self.config.transport();
        transport.check_request(&body)?;
        transport
            .with_retries(request.session_id, request.turn, || self.attempt(&body))
            .await
    }
}

/// The last step for every error of an authenticated request: when the
/// finished message contains the key in either spelling, whether through an
/// endpoint field or by colliding with the harness's own wording, the
/// message is replaced by the empty string and the kind is kept. Only the
/// empty string is safe for every key: any fixed replacement phrase could
/// itself contain an accepted key. Errors without a key are returned as is.
pub(crate) fn scrub(mut error: ProviderError, key: Option<&SecretString>) -> ProviderError {
    if key.is_some_and(|key| Credential::new(key).in_text(&error.message)) {
        error.message = String::new();
    }
    error
}

/// The request body for one completion.
///
/// # Errors
///
/// With `strict_tools`, returns `Rejected` when a tool schema cannot be made
/// strict (see [`strict_input_schema`]).
pub fn request_body(
    config: &AnthropicConfig,
    request: &ModelRequest<'_>,
) -> Result<Value, ProviderError> {
    let mut messages = Vec::with_capacity(request.messages.len());
    for message in request.messages {
        match message {
            Message::User { text } => messages.push(json!({"role": "user", "content": text})),
            Message::Assistant {
                provider_content: Some(content),
                ..
            } if content.provider == PROVIDER_NAME => {
                // Verbatim: preserved thinking refuses an edited turn.
                messages.push(json!({"role": "assistant", "content": content.blocks}));
            }
            Message::Assistant {
                text, tool_calls, ..
            } => {
                // Only a transcript from another provider reaches this: this
                // adapter's own turns always carry their content.
                let mut content = Vec::with_capacity(tool_calls.len() + 1);
                if let Some(text) = text.as_ref().filter(|text| !text.is_empty()) {
                    content.push(json!({"type": "text", "text": text}));
                }
                for call in tool_calls {
                    let input = if call.arguments.is_object() {
                        call.arguments.clone()
                    } else {
                        json!({})
                    };
                    content.push(json!({
                        "type": "tool_use",
                        "id": call.call_id,
                        "name": call.tool,
                        "input": input,
                    }));
                }
                messages.push(json!({"role": "assistant", "content": content}));
            }
            Message::ToolResults { results } => {
                // All of a turn's results go in one user message.
                let content = results
                    .iter()
                    .map(|result| {
                        let ok = result.result.get("ok").and_then(Value::as_bool) == Some(true);
                        json!({
                            "type": "tool_result",
                            "tool_use_id": result.call_id,
                            "content": result.result.to_string(),
                            "is_error": !ok,
                        })
                    })
                    .collect::<Vec<_>>();
                messages.push(json!({"role": "user", "content": content}));
            }
        }
    }
    let mut body = Map::new();
    body.insert("model".to_string(), json!(config.model));
    body.insert("max_tokens".to_string(), json!(request.max_output_tokens));
    if !request.system.is_empty() {
        body.insert("system".to_string(), json!(request.system));
    }
    body.insert("messages".to_string(), Value::Array(messages));
    if !request.tools.is_empty() {
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                let mut entry = Map::new();
                entry.insert("name".to_string(), json!(tool.name));
                entry.insert("description".to_string(), json!(tool.description));
                if config.strict_tools {
                    // The canonical schema stays in ToolDefinition; the
                    // typed tools still enforce every limit it states.
                    let schema = strict_input_schema(&tool.input_schema).map_err(|problem| {
                        ProviderError::new(
                            ProviderErrorKind::Rejected,
                            format!("tool {} cannot be strict: {problem}", tool.name),
                        )
                    })?;
                    entry.insert("input_schema".to_string(), schema);
                    entry.insert("strict".to_string(), json!(true));
                } else {
                    entry.insert("input_schema".to_string(), tool.input_schema.clone());
                }
                Ok(Value::Object(entry))
            })
            .collect::<Result<Vec<_>, ProviderError>>()?;
        body.insert("tools".to_string(), Value::Array(tools));
        body.insert("tool_choice".to_string(), json!({"type": "auto"}));
    }
    body.insert(
        "output_config".to_string(),
        json!({"effort": config.effort.as_str()}),
    );
    // Automatic caching: the breakpoint goes on the last cacheable block.
    body.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
    if config.fallbacks {
        body.insert("fallbacks".to_string(), json!("default"));
    }
    Ok(Value::Object(body))
}

/// Schema keywords sent unchanged in strict mode.
pub const STRICT_KEPT_KEYWORDS: [&str; 11] = [
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "anyOf",
    "enum",
    "const",
    "description",
    "title",
    "default",
];

/// Bounds strict mode refuses that the transform removes. `minItems` is
/// removed only above 1; 0 and 1 are supported and kept. Whoever enforces
/// tool arguments must enforce these locally: the built-in tools' typed
/// validation does.
pub const STRICT_STRIPPED_KEYWORDS: [&str; 8] = [
    "minItems",
    "maxItems",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
];

/// `schema` as strict tool mode accepts it.
///
/// Each subschema is rewritten by keyword: [`STRICT_KEPT_KEYWORDS`] pass
/// through, with `properties`, `items`, `additionalProperties`, and `anyOf`
/// rewritten recursively; [`STRICT_STRIPPED_KEYWORDS`] are removed, except
/// `minItems` of 0 or 1; an object schema without `required` gets
/// `"required": []`, which means the same thing. A property merely named
/// like a keyword is kept.
///
/// # Errors
///
/// Any other keyword (`pattern`, `multipleOf`, `$ref`, `$defs`, `format`,
/// `oneOf`, ...) is refused with its path rather than dropped, since
/// dropping a constraint nothing enforces locally would weaken validation;
/// so are tuple `items` and an object schema without
/// `"additionalProperties": false`, which strict mode requires.
pub fn strict_input_schema(schema: &Value) -> Result<Value, String> {
    strict_schema_at(schema, "input_schema")
}

fn strict_schema_at(schema: &Value, path: &str) -> Result<Value, String> {
    let Some(map) = schema.as_object() else {
        return match schema {
            Value::Bool(_) => Ok(schema.clone()),
            _ => Err(format!("{path} is not a schema")),
        };
    };
    let mut out = Map::new();
    for (keyword, value) in map {
        let here = format!("{path}.{keyword}");
        let value = match keyword.as_str() {
            "minItems" if value.as_u64().is_some_and(|items| items <= 1) => value.clone(),
            keyword if STRICT_STRIPPED_KEYWORDS.contains(&keyword) => continue,
            "properties" => match value {
                Value::Object(entries) => Value::Object(
                    entries
                        .iter()
                        .map(|(name, sub)| {
                            Ok((
                                name.clone(),
                                strict_schema_at(sub, &format!("{here}.{name}"))?,
                            ))
                        })
                        .collect::<Result<Map<_, _>, String>>()?,
                ),
                _ => return Err(format!("{here} is not an object")),
            },
            "items" | "additionalProperties" => strict_schema_at(value, &here)?,
            "anyOf" => match value {
                Value::Array(options) => Value::Array(
                    options
                        .iter()
                        .enumerate()
                        .map(|(index, sub)| strict_schema_at(sub, &format!("{here}[{index}]")))
                        .collect::<Result<Vec<_>, String>>()?,
                ),
                _ => return Err(format!("{here} is not an array")),
            },
            keyword if STRICT_KEPT_KEYWORDS.contains(&keyword) => value.clone(),
            _ => return Err(format!("{here} is not supported in strict mode")),
        };
        out.insert(keyword.clone(), value);
    }
    if out.get("type").and_then(Value::as_str) == Some("object") {
        if out.get("additionalProperties") != Some(&Value::Bool(false)) {
            return Err(format!(
                "{path} is an object schema without \"additionalProperties\": false"
            ));
        }
        out.entry("required").or_insert_with(|| json!([]));
    }
    Ok(Value::Object(out))
}

/// Map a parsed response body to the harness's response.
///
/// # Errors
///
/// Returns `InvalidResponse` for a body that is not a `message` from the
/// `assistant` with a valid `model`, a content array, a known stop reason,
/// and usage; a malformed block; a `tool_use` whose ID, name, or input
/// breaks the rules in the module documentation; malformed `stop_details`;
/// opaque content holding a number that cannot be echoed exactly; or
/// implausible usage. An error object in the body is reported by its
/// allowlisted type only, as for an authenticated request.
pub fn parse_response(value: &Value) -> Result<ModelResponse, ProviderError> {
    parse_response_quoting(value, false)
}

/// [`parse_response`], quoting an error object in the body only when
/// `quote` (a request without a key).
fn parse_response_quoting(value: &Value, quote: bool) -> Result<ModelResponse, ProviderError> {
    // The success envelope comes first: an error object is never read as a
    // message, whatever else it carries.
    match value.get("type").and_then(Value::as_str) {
        Some("message") => {}
        Some("error") => {
            let kind = error_type(value);
            return Err(invalid(if quote {
                let error = value.get("error").map(Value::to_string).unwrap_or_default();
                format!("server reported {kind}: {}", excerpt(error.as_bytes()))
            } else {
                format!("server reported {kind} (body withheld: authenticated request)")
            }));
        }
        _ => return Err(invalid("response type is not \"message\"")),
    }
    if value.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(invalid("response role is not \"assistant\""));
    }
    let model = match value.get("model") {
        Some(Value::String(model)) if model_id_is_valid(model) => model.clone(),
        None | Some(Value::Null) => return Err(invalid("response has no model")),
        Some(_) => return Err(invalid("response model is not a model identifier")),
    };
    let Some(blocks) = value.get("content").and_then(Value::as_array) else {
        return Err(invalid("response has no content array"));
    };
    let mut texts = Vec::new();
    let mut tool_calls = Vec::new();
    for block in blocks {
        let kind = block
            .as_object()
            .and_then(|block| block.get("type"))
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("content block is not an object with a type"))?;
        match kind {
            "text" => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("text block has no text"))?;
                texts.push(text);
            }
            "tool_use" => tool_calls.push(parse_tool_use(block)?),
            // thinking, redacted_thinking, fallback, and anything new live
            // only in the opaque content.
            _ => {}
        }
    }
    let text = texts.join("\n");
    let text = (!text.is_empty()).then_some(text);
    // Only the stop reasons this API version defines for a request without
    // server tools; nothing unknown is taken as a final answer. With client
    // tools only, a tool_use stop and tool_use blocks come together: either
    // without the other is a malformed response, not something to normalize.
    // A truncated or refused turn may hold blocks; the engine refuses it.
    let calls = !tool_calls.is_empty();
    let stop_reason = match value.get("stop_reason").and_then(Value::as_str) {
        Some("tool_use") if calls => StopReason::ToolUse,
        Some("tool_use") => return Err(invalid("tool_use stop with no tool_use block")),
        Some("end_turn" | "stop_sequence") if calls => {
            return Err(invalid("tool_use blocks without a tool_use stop"));
        }
        Some("end_turn" | "stop_sequence") => StopReason::EndTurn,
        Some("max_tokens" | "model_context_window_exceeded") => StopReason::MaxTokens,
        Some("refusal") => StopReason::Refusal,
        Some("pause_turn") => {
            return Err(invalid(
                "pause_turn needs server tools, which this harness does not send",
            ));
        }
        Some(_) => return Err(invalid("unknown stop_reason")),
        None => return Err(invalid("response has no stop_reason")),
    };
    let stop_details = match value.get("stop_details") {
        None | Some(Value::Null) => None,
        Some(details @ Value::Object(_)) => Some(details.clone()),
        Some(_) => return Err(invalid("stop_details is not an object")),
    };
    // Opaque content must come back value for value; serde_json holds
    // only 64-bit integers exactly.
    for block in blocks {
        check_exact_numbers(block)?;
    }
    if let Some(details) = &stop_details {
        check_exact_numbers(details)?;
    }
    let (usage, serving_output) = parse_usage(value.get("usage"), &model)?;
    let response = ModelResponse {
        text,
        tool_calls,
        stop_reason,
        usage,
        provider_content: Some(ProviderContent {
            provider: PROVIDER_NAME.to_string(),
            model: Some(model),
            blocks: Value::Array(blocks.clone()),
            stop_details,
        }),
    };
    // The serving attempt produced the returned content, so its own output
    // count must account for it; declined attempts' output cannot.
    check_completion_tokens(&response, serving_output)?;
    Ok(response)
}

/// Refuse any JSON number `serde_json` does not hold exactly: a float, or an
/// integer outside `i64` and `u64` (parsed through `f64`), anywhere in
/// `value`, with no exemptions.
fn check_exact_numbers(value: &Value) -> Result<(), ProviderError> {
    match value {
        Value::Number(number) if number.as_i64().is_some() || number.as_u64().is_some() => Ok(()),
        Value::Number(_) => Err(invalid(
            "opaque content contains a number that cannot be echoed exactly",
        )),
        Value::Array(items) => items.iter().try_for_each(check_exact_numbers),
        Value::Object(map) => map.values().try_for_each(check_exact_numbers),
        Value::Null | Value::Bool(_) | Value::String(_) => Ok(()),
    }
}

fn parse_tool_use(block: &Value) -> Result<ToolCall, ProviderError> {
    let tool = block
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("tool_use block has no name"))?;
    // Checked before the name is copied anywhere.
    if tool.len() > MAX_TOOL_NAME_BYTES {
        return Err(invalid(format!(
            "tool name exceeds {MAX_TOOL_NAME_BYTES} bytes"
        )));
    }
    // The echoed block keeps this ID, so the result must route to it: an ID
    // the engine would refuse cannot be swapped for a positional one.
    let call_id = block
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| call_id_is_valid(id))
        .ok_or_else(|| invalid("tool_use id is missing or breaks the call-ID rule"))?;
    let arguments = block
        .get("input")
        .filter(|input| input.is_object())
        .ok_or_else(|| invalid("tool_use input is not an object"))?;
    Ok(ToolCall {
        call_id: call_id.to_string(),
        tool: tool.to_string(),
        arguments: arguments.clone(),
    })
}

/// A served model name: 1..=256 bytes of letters, digits, and `._:@/-`.
fn model_id_is_valid(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 256
        && model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:@/-".contains(&byte))
}

/// The response's usage, and the output count of the serving attempt.
///
/// The top-level counters are always checked; they describe the attempt
/// that produced the returned message. With `usage.iterations`, the
/// per-attempt billing record a server-side fallback produces, every entry
/// must have a known `type`, a valid `model`, valid counters, and nonzero
/// input, and the last entry must be the serving attempt: the response's
/// `model` and exactly the top-level counters. Any disagreement is
/// `InvalidResponse`, never resolved by picking a side. The usage is then
/// the saturating sum of every entry, without the top level added again.
fn parse_usage(usage: Option<&Value>, model: &str) -> Result<(Usage, u64), ProviderError> {
    let usage = usage
        .filter(|usage| usage.is_object())
        .ok_or_else(|| invalid("response has no usage; token budgets cannot be enforced"))?;
    let serving = counters(usage, "usage")?;
    let total = match usage.get("iterations") {
        None | Some(Value::Null) => serving,
        Some(Value::Array(attempts)) if !attempts.is_empty() => {
            let mut sum = Usage::default();
            let mut last = None;
            for (index, attempt) in attempts.iter().enumerate() {
                let what = format!("usage iterations[{index}]");
                if !attempt.is_object() {
                    return Err(invalid(format!("{what} is not an object")));
                }
                if !attempt
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|tag| ITERATION_TYPES.contains(&tag))
                {
                    return Err(invalid(format!("{what} has no known type")));
                }
                let attempt_model = attempt
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|model| model_id_is_valid(model))
                    .ok_or_else(|| invalid(format!("{what} has no valid model")))?;
                let counted = counters(attempt, &what)?;
                if counted.total_input_tokens() == 0 {
                    return Err(invalid(format!("{what} reports zero input tokens")));
                }
                sum = sum + counted;
                last = Some((attempt_model, counted));
            }
            match last {
                Some((last_model, counted)) if last_model == model && counted == serving => sum,
                _ => {
                    return Err(invalid(
                        "usage iterations do not end with the serving attempt the top level reports",
                    ));
                }
            }
        }
        Some(_) => return Err(invalid("usage iterations is not a non-empty array")),
    };
    // Every request carries at least the user's task, cached or not; a
    // fully cached prompt has zero uncached input.
    if serving.total_input_tokens() == 0 || total.total_input_tokens() == 0 {
        return Err(invalid("usage reports zero input tokens"));
    }
    Ok((total, serving.output_tokens))
}

/// The four disjoint counters of one usage object; a missing or null
/// counter is 0, and anything else that is not a `u64` is invalid.
fn counters(usage: &Value, what: &str) -> Result<Usage, ProviderError> {
    let count = |field: &str| match usage.get(field) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| invalid(format!("{what} {field} is not a count"))),
    };
    Ok(Usage {
        input_tokens: count("input_tokens")?,
        output_tokens: count("output_tokens")?,
        cache_read_input_tokens: count("cache_read_input_tokens")?,
        cache_creation_input_tokens: count("cache_creation_input_tokens")?,
    })
}

/// `error.type` when it is a known type, otherwise `unknown`.
fn error_type(body: &Value) -> &'static str {
    body.get("error")
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .and_then(|kind| KNOWN_ERROR_TYPES.iter().find(|known| **known == kind))
        .copied()
        .unwrap_or("unknown")
}

/// `request_id` when it is `req_` plus ASCII letters and digits, at most
/// 64 bytes in all.
fn request_id(body: &Value) -> Option<&str> {
    body.get("request_id").and_then(Value::as_str).filter(|id| {
        id.len() <= MAX_REQUEST_ID_BYTES
            && id.strip_prefix("req_").is_some_and(|rest| {
                !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
    })
}

/// Classify an error status. With a key (an authenticated request) the body
/// decides the class but is never quoted: only the allowlisted error type
/// and a validated request ID are, and only when neither holds the key in
/// either spelling; otherwise the message is the bare status. Without a
/// key, a bounded, sanitized excerpt of the body follows them.
fn classify_status(status: StatusCode, body: &[u8], key: Option<&Credential<'_>>) -> Failure {
    let code = status.as_u16();
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let kind = parsed.as_ref().map_or("unknown", error_type);
    let id = parsed.as_ref().and_then(request_id);
    let id_part = id.map(|id| format!(", request {id}")).unwrap_or_default();
    let message = match key {
        None => format!("HTTP {code} {kind}{id_part}: {}", excerpt(body)),
        // The quoted endpoint fields are checked here, and the finished
        // message once more by `scrub`, since fixed wording around them can
        // complete a key too.
        Some(key) if key.in_text(kind) || id.is_some_and(|id| key.in_text(id)) => {
            format!("HTTP {code} (body withheld: authenticated request)")
        }
        Some(_) => format!("HTTP {code} {kind}{id_part} (body withheld: authenticated request)"),
    };
    match code {
        408 => Failure::retryable(ProviderErrorKind::Timeout, message),
        429 => Failure::retryable(ProviderErrorKind::RateLimited, message),
        503 | 529 => Failure::retryable(ProviderErrorKind::Overloaded, message),
        409 | 500 | 502 | 504 => Failure::retryable(ProviderErrorKind::Transport, message),
        // A proxy's or crashed server's error page rather than the API's own
        // JSON error.
        500..=599 if parsed.is_none() => Failure::retryable(ProviderErrorKind::Transport, message),
        500..=599 => Failure::fatal(ProviderErrorKind::Transport, message),
        413 => Failure::fatal(ProviderErrorKind::ContextOverflow, message),
        400 if parsed.as_ref().is_some_and(mentions_context_window) => {
            Failure::fatal(ProviderErrorKind::ContextOverflow, message)
        }
        _ => Failure::fatal(ProviderErrorKind::Rejected, message),
    }
}

/// Whether an `invalid_request_error` is about the prompt's length or the
/// context window. Read to classify, never quoted.
fn mentions_context_window(body: &Value) -> bool {
    let Some(error) = body.get("error") else {
        return false;
    };
    if error.get("type").and_then(Value::as_str) != Some("invalid_request_error") {
        return false;
    }
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    message.contains("prompt is too long")
        || message.contains("context window")
        || message.contains("context length")
        || (message.contains("prompt") && message.contains("too long"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(base: &str) -> AnthropicConfig {
        AnthropicConfig::new(
            base,
            DEFAULT_MODEL,
            Some(SecretString::new("sk-ant-test-key-0123456789")),
        )
        .expect("url")
    }

    #[test]
    fn endpoint_appends_v1_messages() {
        assert_eq!(
            config(DEFAULT_BASE_URL).endpoint().as_str(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            config("http://127.0.0.1:9/proxy/").endpoint().as_str(),
            "http://127.0.0.1:9/proxy/v1/messages"
        );
    }

    #[test]
    fn defaults_follow_the_decisions() {
        let config = config(DEFAULT_BASE_URL);
        config.validate().expect("defaults are valid");
        assert_eq!(config.effort, Effort::High);
        assert!(config.fallbacks);
        assert!(!config.strict_tools);
        assert_eq!(config.model, "claude-opus-5-5");
    }

    #[test]
    fn effort_parses_every_level() {
        for level in ["low", "medium", "high", "xhigh", "max"] {
            assert_eq!(level.parse::<Effort>().expect(level).as_str(), level);
        }
        assert!("HIGH".parse::<Effort>().is_err());
    }

    #[test]
    fn request_ids_are_validated() {
        let id = |value: &str| request_id(&json!({"request_id": value})).map(str::to_string);
        assert_eq!(id("req_011CAbc9"), Some("req_011CAbc9".to_string()));
        for bad in [
            "",
            "req_",
            "req_a-b",
            "id_123",
            "req_\u{e9}",
            &format!("req_{}", "a".repeat(61)),
        ] {
            assert_eq!(id(bad), None, "{bad:?}");
        }
        assert!(id(&format!("req_{}", "a".repeat(60))).is_some());
    }

    #[test]
    fn every_error_message_is_scrubbed_of_the_key() {
        // Keys this short are refused by validation; here they stand in for
        // any key that collides with fixed wording.
        let echo = http::credential_echo().error;
        let key = SecretString::new("request credential");
        let scrubbed = scrub(echo.clone(), Some(&key));
        assert_eq!(scrubbed.message, "");
        assert_eq!(
            scrubbed.kind,
            ProviderErrorKind::Rejected,
            "the kind is kept"
        );
        assert_eq!(
            scrub(echo.clone(), None),
            echo,
            "keyless errors are unchanged"
        );
        let other = SecretString::new("sk-ant-unrelated-key-1234");
        assert_eq!(scrub(echo.clone(), Some(&other)), echo);

        let parse = parse_response(&json!({"type": "completion"})).expect_err("invalid");
        let scrubbed = scrub(parse, Some(&SecretString::new("is not")));
        assert_eq!(
            (scrubbed.kind, scrubbed.message.as_str()),
            (ProviderErrorKind::InvalidResponse, "")
        );
        // The JSON-escaped spelling counts too.
        let quoted = invalid("message holds a\\\\b only escaped");
        assert!(!quoted.message.contains("a\\b"));
        let scrubbed = scrub(quoted, Some(&SecretString::new("a\\b")));
        assert_eq!(scrubbed.message, "");
    }

    #[test]
    fn served_model_names_are_identifiers() {
        assert!(model_id_is_valid("claude-opus-4-8"));
        assert!(model_id_is_valid("anthropic.claude-opus-5-5@v1:0"));
        for bad in ["", "a b", "x\u{1b}[2J", &"m".repeat(257)] {
            assert!(!model_id_is_valid(bad), "{bad:?}");
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one row per status
    fn statuses_are_classified_without_quoting() {
        let key = SecretString::new("sk-ant-test");
        let body = |kind: &str, message: &str| {
            json!({"type": "error", "error": {"type": kind, "message": message},
                   "request_id": "req_42"})
            .to_string()
        };
        let cases: Vec<(u16, String, ProviderErrorKind, bool)> = vec![
            (408, String::new(), ProviderErrorKind::Timeout, true),
            (409, String::new(), ProviderErrorKind::Transport, true),
            (
                429,
                body("rate_limit_error", "slow"),
                ProviderErrorKind::RateLimited,
                true,
            ),
            (
                500,
                body("api_error", "oops"),
                ProviderErrorKind::Transport,
                true,
            ),
            (502, String::new(), ProviderErrorKind::Transport, true),
            (503, String::new(), ProviderErrorKind::Overloaded, true),
            (
                504,
                body("timeout_error", "t"),
                ProviderErrorKind::Transport,
                true,
            ),
            (
                529,
                body("overloaded_error", "busy"),
                ProviderErrorKind::Overloaded,
                true,
            ),
            (
                501,
                "<html>".to_string(),
                ProviderErrorKind::Transport,
                true,
            ),
            (
                501,
                body("api_error", "no"),
                ProviderErrorKind::Transport,
                false,
            ),
            (
                400,
                body(
                    "invalid_request_error",
                    "prompt is too long: 1200000 tokens > 1000000 maximum",
                ),
                ProviderErrorKind::ContextOverflow,
                false,
            ),
            (
                400,
                body("invalid_request_error", "input exceeds the context window"),
                ProviderErrorKind::ContextOverflow,
                false,
            ),
            (
                400,
                body("invalid_request_error", "bad field"),
                ProviderErrorKind::Rejected,
                false,
            ),
            (
                413,
                body("request_too_large", "big"),
                ProviderErrorKind::ContextOverflow,
                false,
            ),
            (
                401,
                body("authentication_error", "no"),
                ProviderErrorKind::Rejected,
                false,
            ),
            (
                402,
                body("billing_error", "pay"),
                ProviderErrorKind::Rejected,
                false,
            ),
            (
                403,
                body("permission_error", "no"),
                ProviderErrorKind::Rejected,
                false,
            ),
            (
                404,
                body("not_found_error", "no"),
                ProviderErrorKind::Rejected,
                false,
            ),
            (302, String::new(), ProviderErrorKind::Rejected, false),
        ];
        for (code, body, kind, retryable) in cases {
            let failure = classify_status(
                StatusCode::from_u16(code).expect("status"),
                body.as_bytes(),
                Some(&Credential::new(&key)),
            );
            assert_eq!(failure.error.kind, kind, "{code} {body}");
            assert_eq!(failure.error.retryable, retryable, "{code} {body}");
            let message = &failure.error.message;
            assert!(message.starts_with(&format!("HTTP {code} ")), "{message}");
            assert!(
                message.ends_with("(body withheld: authenticated request)"),
                "{message}"
            );
            for quoted in [
                "slow",
                "oops",
                "busy",
                "too long",
                "bad field",
                "pay",
                "html",
            ] {
                assert!(!message.contains(quoted), "{message}");
            }
        }
    }
}
