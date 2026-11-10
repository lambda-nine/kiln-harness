//! The HTTP layer every network adapter shares.
//!
//! - A locked-down client: no redirects, no proxy, no response
//!   decompression, so requests go only to the configured endpoint and the
//!   body read is the body sent.
//! - Bounded bodies: the request is checked before it is sent, the response
//!   is read chunk by chunk and abandoned at its cap, and error messages
//!   quote at most [`ERROR_EXCERPT_BYTES`] of a body, without control
//!   characters, and nothing at all once a credential is in play.
//! - The retry loop: full-jitter exponential backoff from a generator seeded
//!   by the session ID and turn, `Retry-After` honored and capped, and every
//!   wait and read dropped as soon as the engine drops the future.
//! - The credential scan for authenticated requests: the raw body, every
//!   decoded JSON string and object key, and everything kept in the parsed
//!   response, for the key and its JSON-escaped spelling.
//! - Configuration rules: an API key needs `https` unless the host is a
//!   loopback IP literal.

use std::borrow::Cow;
use std::future::Future;
use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Client, StatusCode, Url};
use serde_json::Value;

use super::SecretString;
use crate::provider::{ModelResponse, ProviderError, ProviderErrorKind};

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
pub(crate) const ERROR_BODY_READ_BYTES: usize = 64 << 10;
/// Longest configured model name.
pub(crate) const MAX_MODEL_BYTES: usize = 256;

/// Check a base URL. Returns whether it is `https`.
///
/// # Errors
///
/// Refuses a scheme other than `http` or `https`, userinfo, a query, a
/// fragment, or a missing host.
pub(crate) fn check_base_url(url: &Url) -> Result<bool, String> {
    let https = match url.scheme() {
        "https" => true,
        "http" => false,
        other => {
            return Err(format!(
                "base URL scheme must be http or https, not {other:?}"
            ));
        }
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err("base URL must not contain userinfo; pass a key with --api-key-env".into());
    }
    if url.query().is_some() {
        return Err("base URL must not contain a query".into());
    }
    if url.fragment().is_some() {
        return Err("base URL must not contain a fragment".into());
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("base URL must name a host".into());
    }
    Ok(https)
}

/// Check a configured model name: 1..=256 bytes of printable ASCII.
pub(crate) fn check_model(model: &str) -> Result<(), String> {
    if model.is_empty()
        || model.len() > MAX_MODEL_BYTES
        || !model.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
    {
        return Err("model must be 1..=256 bytes of printable ASCII".into());
    }
    Ok(())
}

/// Check an API key and where it may be sent.
pub(crate) fn check_key(key: &SecretString, url: &Url, https: bool) -> Result<(), String> {
    if key.is_empty() || !key.expose().bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("API key must be non-empty printable ASCII without spaces".into());
    }
    if !https && !host_is_loopback(url) {
        return Err(
            "an API key requires an https base URL unless the host is a loopback address".into(),
        );
    }
    Ok(())
}

/// Whether the URL's host is a loopback IP literal. Names, `localhost`
/// included, are not trusted to resolve to loopback.
pub(crate) fn host_is_loopback(url: &Url) -> bool {
    url.host_str()
        .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
        .and_then(|host| host.parse::<IpAddr>().ok())
        .is_some_and(|address| address.is_loopback())
}

/// Body caps, retry policy, and connect timeout, as every adapter configures
/// them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Transport {
    pub connect_timeout: Duration,
    pub max_response_bytes: usize,
    pub max_request_bytes: usize,
    pub max_attempts: u32,
    pub retry_base_delay: Duration,
    pub retry_max_delay: Duration,
}

impl Transport {
    /// Check every bound and delay.
    pub(crate) fn check(&self) -> Result<(), String> {
        for (name, bytes) in [
            ("max_response_bytes", self.max_response_bytes),
            ("max_request_bytes", self.max_request_bytes),
        ] {
            if bytes == 0 || bytes > MAX_BODY_BYTES {
                return Err(format!("{name} must be within 1..={MAX_BODY_BYTES}"));
            }
        }
        if !(1..=MAX_ATTEMPTS).contains(&self.max_attempts) {
            return Err(format!("max_attempts must be within 1..={MAX_ATTEMPTS}"));
        }
        for (name, delay) in [
            ("connect_timeout", self.connect_timeout),
            ("retry_base_delay", self.retry_base_delay),
            ("retry_max_delay", self.retry_max_delay),
        ] {
            if delay.is_zero() || delay > MAX_CONFIG_DELAY {
                return Err(format!(
                    "{name} must be positive and at most {} s",
                    MAX_CONFIG_DELAY.as_secs()
                ));
            }
        }
        if self.retry_max_delay < self.retry_base_delay {
            return Err("retry_max_delay must be at least retry_base_delay".into());
        }
        Ok(())
    }

    /// The HTTP client: no redirects, no proxy, no decompression.
    pub(crate) fn client(&self) -> Result<Client, String> {
        Client::builder()
            .connect_timeout(self.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent(concat!("kiln-harness/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("HTTP client: {error}"))
    }

    /// Refuse a serialized request over `max_request_bytes` before sending.
    pub(crate) fn check_request(&self, body: &[u8]) -> Result<(), ProviderError> {
        if body.len() > self.max_request_bytes {
            return Err(ProviderError::new(
                ProviderErrorKind::ContextOverflow,
                format!(
                    "request is {} bytes; the limit is {}",
                    body.len(),
                    self.max_request_bytes
                ),
            ));
        }
        Ok(())
    }

    /// Run `attempt` until it succeeds, fails without `retryable`, or
    /// `max_attempts` run out, sleeping [`retry_delay`] between attempts.
    /// Dropping the returned future drops the attempt in flight and any
    /// backoff sleep.
    pub(crate) async fn with_retries<F, Fut>(
        &self,
        session_id: &str,
        turn: u32,
        mut attempt: F,
    ) -> Result<ModelResponse, ProviderError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<ModelResponse, Failure>>,
    {
        let mut jitter = Jitter::new(session_id, turn);
        let mut number = 1;
        loop {
            let failure = match attempt().await {
                Ok(response) => return Ok(response),
                Err(failure) => failure,
            };
            if !failure.error.retryable || number >= self.max_attempts {
                return Err(failure.error);
            }
            let delay = retry_delay(self, number, failure.retry_after, &mut jitter);
            tokio::time::sleep(delay).await;
            number += 1;
        }
    }
}

/// One HTTP exchange's outcome before the adapter classifies it.
pub(crate) struct Exchange {
    pub status: StatusCode,
    pub retry_after: Option<Duration>,
    /// At most the success or error read limit.
    pub body: Vec<u8>,
    /// Whether the whole body fit in its limit.
    pub complete: bool,
}

/// POST `body` and read the bounded response: up to `max_response_bytes`
/// for a success, and up to [`ERROR_BODY_READ_BYTES`] of that for an error.
pub(crate) async fn post(
    client: &Client,
    endpoint: &Url,
    headers: HeaderMap,
    body: &[u8],
    max_response_bytes: usize,
) -> Result<Exchange, Failure> {
    let mut response = client
        .post(endpoint.clone())
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
    let limit = if status.is_success() {
        max_response_bytes
    } else {
        max_response_bytes.min(ERROR_BODY_READ_BYTES)
    };
    let (body, complete) = read_bounded(&mut response, limit).await?;
    Ok(Exchange {
        status,
        retry_after,
        body,
        complete,
    })
}

/// A successful body as JSON: complete within `limit`, and, with a key,
/// free of it in the raw bytes and in every decoded string and object key.
pub(crate) fn decode_success(
    exchange: &Exchange,
    limit: usize,
    key: Option<&Credential<'_>>,
) -> Result<Value, Failure> {
    if !exchange.complete {
        return Err(Failure::fatal(
            ProviderErrorKind::InvalidResponse,
            format!("response exceeds {limit} bytes"),
        ));
    }
    if key.is_some_and(|key| key.in_bytes(&exchange.body)) {
        return Err(credential_echo());
    }
    let value: Value = serde_json::from_slice(&exchange.body).map_err(|error| {
        Failure::fatal(
            ProviderErrorKind::InvalidResponse,
            format!("response is not JSON: {error}"),
        )
    })?;
    // Decoded strings catch the key behind the outer JSON layer.
    if key.is_some_and(|key| key.in_value(&value)) {
        return Err(credential_echo());
    }
    Ok(value)
}

/// The refusal for a response that contains the request's credential.
/// Nothing from the response is kept.
pub(crate) fn credential_echo() -> Failure {
    Failure::fatal(
        ProviderErrorKind::Rejected,
        "response contained the request credential",
    )
}

/// The request's API key as the response scans look for it: its exact
/// bytes and its JSON-escaped spelling (which differs when the key holds
/// `"` or `\\`). Both must be absent from every layer the protocol decodes.
pub(crate) struct Credential<'a> {
    needles: Vec<Cow<'a, str>>,
}

impl<'a> Credential<'a> {
    pub(crate) fn new(key: &'a SecretString) -> Self {
        let exact = key.expose();
        let mut needles = vec![Cow::Borrowed(exact)];
        // Encoding a str cannot fail; strip the surrounding quotes.
        let encoded = serde_json::to_string(exact).unwrap_or_default();
        let escaped = encoded
            .strip_prefix('"')
            .and_then(|inner| inner.strip_suffix('"'))
            .unwrap_or(exact);
        if escaped != exact {
            needles.push(Cow::Owned(escaped.to_string()));
        }
        needles.retain(|needle| !needle.is_empty());
        Self { needles }
    }

    pub(crate) fn in_text(&self, text: &str) -> bool {
        self.needles
            .iter()
            .any(|needle| text.contains(needle.as_ref()))
    }

    pub(crate) fn in_bytes(&self, bytes: &[u8]) -> bool {
        self.needles.iter().any(|needle| {
            let needle = needle.as_bytes();
            bytes.windows(needle.len()).any(|window| window == needle)
        })
    }

    /// Any string or object key in `value`.
    pub(crate) fn in_value(&self, value: &Value) -> bool {
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
    pub(crate) fn in_response(&self, response: &ModelResponse) -> bool {
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
            || response.provider_content.as_ref().is_some_and(|content| {
                content
                    .model
                    .as_ref()
                    .is_some_and(|model| self.in_text(model))
                    || self.in_value(&content.blocks)
                    || content
                        .stop_details
                        .as_ref()
                        .is_some_and(|details| self.in_value(details))
            })
    }
}

/// A failed attempt and the server's `Retry-After`, if it sent one.
#[derive(Debug)]
pub(crate) struct Failure {
    pub error: ProviderError,
    pub retry_after: Option<Duration>,
}

impl Failure {
    pub(crate) fn fatal(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            error: ProviderError::new(kind, message),
            retry_after: None,
        }
    }

    pub(crate) fn retryable(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        let mut error = ProviderError::new(kind, message);
        error.retryable = true;
        Self {
            error,
            retry_after: None,
        }
    }

    /// Every transport failure is retryable: the request may not have
    /// reached the server, and a completion has no side effects to repeat.
    pub(crate) fn transport(error: &reqwest::Error, action: &str) -> Self {
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

impl From<ProviderError> for Failure {
    fn from(error: ProviderError) -> Self {
        Self {
            error,
            retry_after: None,
        }
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

/// At most [`ERROR_EXCERPT_BYTES`] of `bytes`, UTF-8-lossy, without control
/// characters.
pub(crate) fn excerpt(bytes: &[u8]) -> String {
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
    transport: &Transport,
    attempt: u32,
    retry_after: Option<Duration>,
    jitter: &mut Jitter,
) -> Duration {
    if let Some(retry_after) = retry_after {
        return retry_after.min(transport.retry_max_delay);
    }
    let factor = 1_u32
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u32::MAX);
    let ceiling = transport
        .retry_base_delay
        .saturating_mul(factor)
        .min(transport.retry_max_delay);
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

/// Refuse `completion`, the output-token count of the attempt that produced
/// `response`'s content, when it cannot be right for the output kept: zero
/// for non-empty output (opaque content blocks included), or fewer than one
/// token per [`MAX_OUTPUT_BYTES_PER_TOKEN`] bytes of
/// [`ModelResponse::token_floor_bytes`]. Real tokenizers average a few bytes
/// per token and rarely exceed 16, so the floor only catches gross
/// under-reporting. Opaque content stays out of the floor, since a thinking
/// signature is large and unrelated to completion tokens; the engine bounds
/// all output, opaque content included, separately in bytes.
pub(crate) fn check_completion_tokens(
    response: &ModelResponse,
    completion: u64,
) -> Result<(), ProviderError> {
    let output_bytes = response.token_floor_bytes();
    let opaque = response.provider_content.as_ref().is_some_and(|content| {
        content
            .blocks
            .as_array()
            .is_none_or(|blocks| !blocks.is_empty())
    });
    if (output_bytes > 0 || opaque) && completion == 0 {
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

pub(crate) fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidResponse, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport() -> Transport {
        Transport {
            connect_timeout: Duration::from_secs(10),
            max_response_bytes: 4 << 20,
            max_request_bytes: 8 << 20,
            max_attempts: 3,
            retry_base_delay: Duration::from_millis(500),
            retry_max_delay: Duration::from_secs(8),
        }
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
        let config = transport();
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
}
