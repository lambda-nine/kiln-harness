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

use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::{Client, StatusCode, Url};
use serde_json::{Map, Value, json};
use thiserror::Error;

use super::SecretString;
use crate::engine::call_id_is_valid;
use crate::provider::{
    Message, ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind, ProviderInfo,
    StopReason, ToolCall, Usage,
};

/// The name recorded in journals for this adapter.
pub const PROVIDER_NAME: &str = "openai-compat";
/// Ceiling for `max_response_bytes` and `max_request_bytes`.
pub const MAX_BODY_BYTES: usize = 64 << 20;
/// Ceiling for `max_attempts`.
pub const MAX_ATTEMPTS: u32 = 8;
/// Ceiling for `connect_timeout`, `retry_base_delay`, and `retry_max_delay`.
pub const MAX_CONFIG_DELAY: Duration = Duration::from_secs(300);
/// Most bytes of an error body quoted in an error message.
pub const ERROR_EXCERPT_BYTES: usize = 512;
/// Longest tool name accepted from a response.
pub const MAX_TOOL_NAME_BYTES: usize = 256;
/// The most output bytes one reported completion token may account for.
pub const MAX_OUTPUT_BYTES_PER_TOKEN: u64 = 64;
/// Most bytes of an error response read to classify it.
const ERROR_BODY_READ_BYTES: usize = 64 << 10;
const MAX_MODEL_BYTES: usize = 256;

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
        let url = &self.base_url;
        let https = match url.scheme() {
            "https" => true,
            "http" => false,
            other => {
                return Err(config_error(format!(
                    "base URL scheme must be http or https, not {other:?}"
                )));
            }
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err(config_error(
                "base URL must not contain userinfo; pass a key with --api-key-env",
            ));
        }
        if url.query().is_some() {
            return Err(config_error("base URL must not contain a query"));
        }
        if url.fragment().is_some() {
            return Err(config_error("base URL must not contain a fragment"));
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err(config_error("base URL must name a host"));
        }
        if self.model.is_empty()
            || self.model.len() > MAX_MODEL_BYTES
            || !self.model.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        {
            return Err(config_error(
                "model must be 1..=256 bytes of printable ASCII",
            ));
        }
        if let Some(key) = &self.api_key {
            if key.is_empty() || !key.expose().bytes().all(|byte| byte.is_ascii_graphic()) {
                return Err(config_error(
                    "API key must be non-empty printable ASCII without spaces",
                ));
            }
            if !https && !host_is_loopback(url) {
                return Err(config_error(
                    "an API key requires an https base URL unless the host is a loopback address",
                ));
            }
        }
        if !self.temperature.is_finite() || !(0.0..=2.0).contains(&self.temperature) {
            return Err(config_error("temperature must be within 0.0..=2.0"));
        }
        for (name, bytes) in [
            ("max_response_bytes", self.max_response_bytes),
            ("max_request_bytes", self.max_request_bytes),
        ] {
            if bytes == 0 || bytes > MAX_BODY_BYTES {
                return Err(config_error(format!(
                    "{name} must be within 1..={MAX_BODY_BYTES}"
                )));
            }
        }
        if !(1..=MAX_ATTEMPTS).contains(&self.max_attempts) {
            return Err(config_error(format!(
                "max_attempts must be within 1..={MAX_ATTEMPTS}"
            )));
        }
        for (name, delay) in [
            ("connect_timeout", self.connect_timeout),
            ("retry_base_delay", self.retry_base_delay),
            ("retry_max_delay", self.retry_max_delay),
        ] {
            if delay.is_zero() || delay > MAX_CONFIG_DELAY {
                return Err(config_error(format!(
                    "{name} must be positive and at most {} s",
                    MAX_CONFIG_DELAY.as_secs()
                )));
            }
        }
        if self.retry_max_delay < self.retry_base_delay {
            return Err(config_error(
                "retry_max_delay must be at least retry_base_delay",
            ));
        }
        Ok(())
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

/// Whether the URL's host is a loopback IP literal. Names, `localhost`
/// included, are not trusted to resolve to loopback.
fn host_is_loopback(url: &Url) -> bool {
    url.host_str()
        .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
        .and_then(|host| host.parse::<IpAddr>().ok())
        .is_some_and(|address| address.is_loopback())
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
        let client = Client::builder()
            .connect_timeout(config.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent(concat!("kiln-harness/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| config_error(format!("HTTP client: {error}")))?;
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
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .headers(headers)
            .body(body.to_vec())
            .send()
            .await
            .map_err(|error| Failure::transport(&error, "send request"))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_retry_after(value, SystemTime::now()));
        // With a key, the endpoint holds a credential: nothing it sends is
        // quoted, and a response that contains the key is refused whole.
        let key = self.config.api_key.as_ref().map(Credential::new);
        let key = key.as_ref();
        if status.is_success() {
            let limit = self.config.max_response_bytes;
            let (bytes, complete) = read_bounded(&mut response, limit).await?;
            if !complete {
                return Err(Failure::fatal(
                    ProviderErrorKind::InvalidResponse,
                    format!("response exceeds {limit} bytes"),
                ));
            }
            if key.is_some_and(|key| key.in_bytes(&bytes)) {
                return Err(credential_echo());
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
                Failure::fatal(
                    ProviderErrorKind::InvalidResponse,
                    format!("response is not JSON: {error}"),
                )
            })?;
            // Decoded strings catch the key behind the outer JSON layer.
            if key.is_some_and(|key| key.in_value(&value)) {
                return Err(credential_echo());
            }
            let parsed =
                parse_response_quoting(&value, turn, key.is_none()).map_err(|error| Failure {
                    error,
                    retry_after: None,
                })?;
            // Tool arguments are JSON inside a JSON string: a second layer.
            if key.is_some_and(|key| key.in_response(&parsed)) {
                return Err(credential_echo());
            }
            return Ok(parsed);
        }
        let limit = self.config.max_response_bytes.min(ERROR_BODY_READ_BYTES);
        let (bytes, _) = read_bounded(&mut response, limit).await?;
        let mut failure = classify_status(status, &bytes, key.is_none());
        failure.retry_after = retry_after;
        Err(failure)
    }
}

/// The refusal for a response that contains the request's credential.
/// Nothing from the response is kept.
fn credential_echo() -> Failure {
    Failure::fatal(
        ProviderErrorKind::Rejected,
        "response contained the request credential",
    )
}

/// The request's API key as the response scans look for it: its exact
/// bytes and its JSON-escaped spelling (which differs when the key holds
/// `"` or `\\`). Both must be absent from every layer the protocol decodes.
struct Credential<'a> {
    needles: Vec<std::borrow::Cow<'a, str>>,
}

impl<'a> Credential<'a> {
    fn new(key: &'a SecretString) -> Self {
        let exact = key.expose();
        let mut needles = vec![std::borrow::Cow::Borrowed(exact)];
        // Encoding a str cannot fail; strip the surrounding quotes.
        let encoded = serde_json::to_string(exact).unwrap_or_default();
        let escaped = encoded
            .strip_prefix('"')
            .and_then(|inner| inner.strip_suffix('"'))
            .unwrap_or(exact);
        if escaped != exact {
            needles.push(std::borrow::Cow::Owned(escaped.to_string()));
        }
        needles.retain(|needle| !needle.is_empty());
        Self { needles }
    }

    fn in_text(&self, text: &str) -> bool {
        self.needles
            .iter()
            .any(|needle| text.contains(needle.as_ref()))
    }

    fn in_bytes(&self, bytes: &[u8]) -> bool {
        self.needles.iter().any(|needle| {
            let needle = needle.as_bytes();
            bytes.windows(needle.len()).any(|window| window == needle)
        })
    }

    /// Any string or object key in `value`.
    fn in_value(&self, value: &Value) -> bool {
        match value {
            Value::String(text) => self.in_text(text),
            Value::Array(items) => items.iter().any(|item| self.in_value(item)),
            Value::Object(map) => map
                .iter()
                .any(|(name, item)| self.in_text(name) || self.in_value(item)),
            Value::Null | Value::Bool(_) | Value::Number(_) => false,
        }
    }

    /// Anything kept from `response`. Arguments kept as a raw string are
    /// also decoded when they are JSON of any kind (a string, array, number
    /// or null as well as an object), so the protocol's nested JSON layer
    /// cannot carry the key in its encoded spelling.
    fn in_response(&self, response: &ModelResponse) -> bool {
        response
            .text
            .as_ref()
            .is_some_and(|text| self.in_text(text))
            || response.tool_calls.iter().any(|call| {
                self.in_text(&call.call_id)
                    || self.in_text(&call.tool)
                    || self.in_value(&call.arguments)
                    || matches!(&call.arguments, Value::String(raw)
                        if serde_json::from_str::<Value>(raw)
                            .is_ok_and(|decoded| self.in_value(&decoded)))
            })
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
        if body.len() > self.config.max_request_bytes {
            return Err(ProviderError::new(
                ProviderErrorKind::ContextOverflow,
                format!(
                    "request is {} bytes; the limit is {}",
                    body.len(),
                    self.config.max_request_bytes
                ),
            ));
        }
        let mut jitter = Jitter::new(request.session_id, request.turn);
        let mut attempt = 1;
        loop {
            let failure = match self.attempt(&body, request.turn).await {
                Ok(response) => return Ok(response),
                Err(failure) => failure,
            };
            if !failure.error.retryable || attempt >= self.config.max_attempts {
                return Err(failure.error);
            }
            let delay = retry_delay(&self.config, attempt, failure.retry_after, &mut jitter);
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }
}

/// A failed attempt and the server's `Retry-After`, if it sent one.
#[derive(Debug)]
struct Failure {
    error: ProviderError,
    retry_after: Option<Duration>,
}

impl Failure {
    fn fatal(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            error: ProviderError::new(kind, message),
            retry_after: None,
        }
    }

    fn retryable(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        let mut error = ProviderError::new(kind, message);
        error.retryable = true;
        Self {
            error,
            retry_after: None,
        }
    }

    /// Every transport failure is retryable: the request may not have
    /// reached the server, and a completion has no side effects to repeat.
    fn transport(error: &reqwest::Error, action: &str) -> Self {
        let kind = if error.is_timeout() {
            ProviderErrorKind::Timeout
        } else {
            ProviderErrorKind::Transport
        };
        let what = if error.is_connect() {
            "connect"
        } else {
            action
        };
        Self::retryable(kind, excerpt(error_chain(error).as_bytes())).with_prefix(what)
    }

    fn with_prefix(mut self, prefix: &str) -> Self {
        self.error.message = format!("{prefix} failed: {}", self.error.message);
        self
    }
}

/// An error and its sources, without the request URL.
fn error_chain(error: &reqwest::Error) -> String {
    let mut text = String::new();
    let mut source: Option<&dyn std::error::Error> = Some(error);
    let mut first = true;
    while let Some(current) = source {
        let part = if first {
            // reqwest's own message names the URL, which is configuration,
            // not something the journal needs.
            match error.url() {
                Some(url) => current.to_string().replace(url.as_str(), "<endpoint>"),
                None => current.to_string(),
            }
        } else {
            current.to_string()
        };
        if !text.contains(&part) {
            if !first {
                text.push_str(": ");
            }
            text.push_str(&part);
        }
        first = false;
        source = current.source();
    }
    text
}

/// Read at most `limit` bytes of the body. Returns the bytes and whether the
/// whole body fit; reading stops at the first chunk that would cross the
/// limit, and a declared length over it is refused before any read.
async fn read_bounded(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<(Vec<u8>, bool), Failure> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Ok((Vec::new(), false));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| Failure::transport(&error, "read response"))?
    {
        let room = limit - bytes.len();
        if chunk.len() > room {
            bytes.extend_from_slice(&chunk[..room]);
            return Ok((bytes, false));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, true))
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

/// At most [`ERROR_EXCERPT_BYTES`] of `bytes`, UTF-8-lossy, without control
/// characters.
fn excerpt(bytes: &[u8]) -> String {
    let head = &bytes[..bytes.len().min(ERROR_EXCERPT_BYTES)];
    let mut text = String::new();
    for character in String::from_utf8_lossy(head).chars() {
        if character.is_control() {
            continue;
        }
        if text.len() + character.len_utf8() > ERROR_EXCERPT_BYTES {
            break;
        }
        text.push(character);
    }
    text
}

/// The delay before attempt `attempt + 1`: the server's `Retry-After` when it
/// sent one, otherwise full jitter over exponential backoff, both capped at
/// `retry_max_delay`.
fn retry_delay(
    config: &OpenAiCompatConfig,
    attempt: u32,
    retry_after: Option<Duration>,
    jitter: &mut Jitter,
) -> Duration {
    if let Some(retry_after) = retry_after {
        return retry_after.min(config.retry_max_delay);
    }
    let factor = 1_u32
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u32::MAX);
    let ceiling = config
        .retry_base_delay
        .saturating_mul(factor)
        .min(config.retry_max_delay);
    let ceiling_ms = u64::try_from(ceiling.as_millis()).unwrap_or(u64::MAX);
    Duration::from_millis(jitter.next() % ceiling_ms.saturating_add(1))
}

/// A xorshift64* generator seeded from the session ID and turn, so retry
/// delays are reproducible within a session and differ between sessions.
struct Jitter(u64);

impl Jitter {
    fn new(session_id: &str, turn: u32) -> Self {
        // FNV-1a over the session ID, then the turn.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in session_id.bytes().chain(turn.to_le_bytes()) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self(if hash == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            hash
        })
    }

    fn next(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        self.0 = state;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// `Retry-After` as delta-seconds or an IMF-fixdate HTTP date. Other forms
/// are ignored and the jittered backoff applies.
fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        // Anything too long for u64 is far beyond the cap anyway.
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    let at = parse_http_date(value)?;
    let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(at.saturating_sub(now)))
}

/// Seconds since the Unix epoch for an IMF-fixdate such as
/// `Sun, 06 Nov 1994 08:49:37 GMT`.
fn parse_http_date(value: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = value.split(' ');
    let weekday = parts.next()?;
    let day = parts.next()?;
    let month = parts.next()?;
    let year = parts.next()?;
    let time = parts.next()?;
    if parts.next()? != "GMT" || parts.next().is_some() {
        return None;
    }
    if weekday.len() != 4 || !weekday.ends_with(',') || day.len() != 2 || year.len() != 4 {
        return None;
    }
    let digits = |text: &str| -> Option<u64> {
        if text.bytes().all(|byte| byte.is_ascii_digit()) {
            text.parse().ok()
        } else {
            None
        }
    };
    let day = digits(day)?;
    let month = MONTHS.iter().position(|name| *name == month)? as u64 + 1;
    let year = digits(year)?;
    let mut clock = time.split(':');
    let (hour, minute, second) = (
        digits(clock.next()?)?,
        digits(clock.next()?)?,
        digits(clock.next()?)?,
    );
    if clock.next().is_some() || time.len() != 8 {
        return None;
    }
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 || year < 1970 {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days from 1970-01-01 to the given proleptic Gregorian date (Howard
/// Hinnant's algorithm), for years from 1970.
fn days_from_civil(year: u64, month: u64, day: u64) -> Option<u64> {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    (era * 146_097 + day_of_era).checked_sub(719_468)
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
            Message::Assistant { text, tool_calls } => {
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

fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidResponse, message)
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
    };
    check_completion_tokens(&response)?;
    Ok(response)
}

/// Refuse a completion count that cannot be right for the output kept: zero
/// for non-empty output, or fewer than one token per
/// [`MAX_OUTPUT_BYTES_PER_TOKEN`] bytes of it. Real tokenizers average a few
/// bytes per token and rarely exceed 16, so the floor only catches gross
/// under-reporting. Token counts are trusted for cost; the engine bounds
/// output size separately in bytes.
fn check_completion_tokens(response: &ModelResponse) -> Result<(), ProviderError> {
    let output_bytes = response.output_bytes();
    let completion = response.usage.output_tokens;
    if output_bytes > 0 && completion == 0 {
        return Err(invalid(
            "usage reports zero completion tokens for non-empty output",
        ));
    }
    if completion.saturating_mul(MAX_OUTPUT_BYTES_PER_TOKEN) < output_bytes {
        return Err(invalid(format!(
            "usage reports {completion} completion tokens for {output_bytes} bytes of output, \
             more than {MAX_OUTPUT_BYTES_PER_TOKEN} bytes per token"
        )));
    }
    Ok(())
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
    fn retry_after_accepts_seconds_and_http_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(784_111_777); // Sun, 06 Nov 1994 08:49:37 GMT
        assert_eq!(parse_retry_after("7", now), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:47 GMT", now),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:00 GMT", now),
            Some(Duration::ZERO),
            "a date in the past means retry now"
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_http_date("Tue, 29 Feb 2028 12:00:00 GMT"),
            Some(1_835_438_400)
        );
        for bad in [
            "",
            "-1",
            "1.5",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, 6 Nov 1994 08:49:37 GMT",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
        ] {
            assert_eq!(parse_retry_after(bad, now), None, "{bad:?}");
        }
        assert_eq!(
            parse_retry_after("99999999999999999999999", now),
            Some(Duration::from_secs(u64::MAX))
        );
    }

    #[test]
    fn backoff_is_full_jitter_capped_and_reproducible() {
        let config = config();
        let delays = |session: &str| {
            let mut jitter = Jitter::new(session, 1);
            (1..=8)
                .map(|attempt| retry_delay(&config, attempt, None, &mut jitter))
                .collect::<Vec<_>>()
        };
        let first = delays("hs-1");
        assert_eq!(first, delays("hs-1"));
        assert_ne!(first, delays("hs-2"));
        for (index, delay) in first.iter().enumerate() {
            let ceiling = config
                .retry_base_delay
                .saturating_mul(1 << index)
                .min(config.retry_max_delay);
            assert!(*delay <= ceiling, "{delay:?} > {ceiling:?}");
        }
        let mut jitter = Jitter::new("hs-1", 1);
        assert_eq!(
            retry_delay(&config, 1, Some(Duration::from_secs(3600)), &mut jitter),
            config.retry_max_delay
        );
    }

    #[test]
    fn excerpts_are_bounded_and_printable() {
        let mut body = b"bad\x00\x1b[31m\r\nrequest ".to_vec();
        body.extend(std::iter::repeat_n(0xe2_u8, 2000));
        let text = excerpt(&body);
        assert!(text.len() <= ERROR_EXCERPT_BYTES);
        assert!(text.starts_with("bad[31mrequest "), "{text:?}");
        assert!(!text.chars().any(char::is_control));
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
