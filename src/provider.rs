use std::collections::VecDeque;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// One tool invocation requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    /// Provider-assigned ID, used only to route the result back to the model.
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
}

impl ToolCall {
    #[must_use]
    pub fn new(call_id: impl Into<String>, tool: impl Into<String>, arguments: Value) -> Self {
        Self {
            call_id: call_id.into(),
            tool: tool.into(),
            arguments,
        }
    }
}

/// The model-visible result of one tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub tool: String,
    pub result: Value,
}

/// One entry in the provider-neutral conversation sent to the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        text: String,
    },
    Assistant {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    ToolResults {
        results: Vec<ToolResult>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Other,
}

/// Token usage for one response.
///
/// The three input counters are disjoint: `input_tokens` counts uncached
/// input only, and cache reads and cache writes are counted separately.
/// Adapters must normalize to this. For example, an API that reports total
/// prompt tokens with a cached subset must subtract the cached count from its
/// prompt tokens before filling `input_tokens`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    /// All input tokens: uncached, cache-read, and cache-creation, saturating.
    #[must_use]
    pub fn total_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }
}

/// Adds two usage reports, saturating each counter at `u64::MAX`.
impl std::ops::Add for Usage {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(other.cache_read_input_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_add(other.cache_creation_input_tokens),
        }
    }
}

/// One complete model turn: optional text plus zero or more tool calls.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelResponse {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

impl ModelResponse {
    /// A final answer that ends the turn.
    #[must_use]
    pub fn final_text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    /// A turn that requests the given tool calls.
    #[must_use]
    pub fn tool_use(tool_calls: impl IntoIterator<Item = ToolCall>) -> Self {
        Self {
            text: None,
            tool_calls: tool_calls.into_iter().collect(),
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        }
    }
}

/// Everything a provider needs for one completion. It carries no credentials.
#[derive(Debug, Clone, Copy)]
pub struct ModelRequest<'a> {
    pub session_id: &'a str,
    pub turn: u32,
    pub system: &'a str,
    pub tools: &'a [ToolDefinition],
    pub messages: &'a [Message],
    pub max_output_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderInfo {
    pub name: String,
    pub model: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Transport,
    Timeout,
    RateLimited,
    Overloaded,
    Rejected,
    InvalidResponse,
    ContextOverflow,
    Script,
    ReplayDiverged,
}

impl ProviderErrorKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::RateLimited => "rate_limited",
            Self::Overloaded => "overloaded",
            Self::Rejected => "rejected",
            Self::InvalidResponse => "invalid_response",
            Self::ContextOverflow => "context_overflow",
            Self::Script => "script",
            Self::ReplayDiverged => "replay_diverged",
        }
    }
}

impl fmt::Display for ProviderErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A provider failure. The loop treats every provider error as fatal; any
/// retry policy belongs inside a network adapter.
#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[error("provider failed ({kind}): {message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retryable: bool,
}

impl ProviderError {
    #[must_use]
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: false,
        }
    }
}

#[async_trait]
pub trait Provider: Send {
    fn info(&self) -> ProviderInfo;

    /// Answer one request.
    ///
    /// TODO(network adapters): bound the provider's response body in bytes
    /// before parsing it. The loop and the journal bound what they keep and
    /// write, but a response is fully in memory by the time it returns here.
    async fn complete(&mut self, request: ModelRequest<'_>)
    -> Result<ModelResponse, ProviderError>;

    /// Observe one completed tool call, after it is journaled and before its
    /// result goes back to the model. `call` is the sanitized call. Live
    /// providers ignore this; replay compares it with the recording, and an
    /// error ends the session as a provider error.
    ///
    /// # Errors
    ///
    /// Returns an error to stop the session at this tool call.
    fn tool_completed(
        &mut self,
        _turn: u32,
        _index: u32,
        _call: &ToolCall,
        _ok: bool,
        _result: &Value,
    ) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// The bytes the demo writes, reads back, and counts.
pub const DEMO_CONTENT: &str = "hello from kiln-harness\n";

/// A deterministic provider used by tests and the no-API demo.
pub struct ScriptedProvider {
    responses: VecDeque<ModelResponse>,
    validate_demo_results: bool,
}

impl ScriptedProvider {
    #[must_use]
    pub fn new(responses: impl IntoIterator<Item = ModelResponse>) -> Self {
        Self {
            responses: responses.into_iter().collect(),
            validate_demo_results: false,
        }
    }

    /// Five turns, including one with two calls, then a verified final answer.
    #[must_use]
    pub fn demo() -> Self {
        let mut provider = Self::new([
            ModelResponse::tool_use([ToolCall::new(
                "call-1",
                "write_file",
                json!({"path": "demo.txt", "content": DEMO_CONTENT}),
            )]),
            ModelResponse::tool_use([
                ToolCall::new(
                    "call-2",
                    "read_file",
                    json!({"path": "demo.txt", "max_bytes": 4096}),
                ),
                ToolCall::new("call-3", "list_dir", json!({"path": "."})),
            ]),
            ModelResponse::tool_use([ToolCall::new("call-4", "workspace_diff", json!({}))]),
            ModelResponse::tool_use([ToolCall::new(
                "call-5",
                "exec",
                json!({
                    "argv": ["/usr/bin/wc", "-c", "/workspace/demo.txt"],
                    "stdin": ""
                }),
            )]),
            ModelResponse::final_text(
                "The deterministic harness wrote, read, listed, diffed, and counted demo.txt through kilnd.",
            ),
        ]);
        provider.validate_demo_results = true;
        provider
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "scripted".to_string(),
            model: "demo".to_string(),
        }
    }

    async fn complete(
        &mut self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        if request.tools.is_empty() {
            return Err(script_error("tool registry is empty"));
        }
        if self.validate_demo_results {
            validate_demo_results(&request)?;
        }
        self.responses
            .pop_front()
            .ok_or_else(|| script_error("script ended before a final answer"))
    }
}

fn script_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Script, message)
}

fn validate_demo_results(request: &ModelRequest<'_>) -> Result<(), ProviderError> {
    let expected: &[(&str, &str)] = match request.turn {
        1 => return Ok(()),
        2 => &[("call-1", "write_file")],
        3 => &[("call-2", "read_file"), ("call-3", "list_dir")],
        4 => &[("call-4", "workspace_diff")],
        5 => &[("call-5", "exec")],
        _ => return Err(script_error("demo received an unexpected turn")),
    };
    let Some(Message::ToolResults { results }) = request.messages.last() else {
        return Err(script_error(
            "demo expected tool results before the next turn",
        ));
    };
    if results.len() != expected.len() {
        return Err(script_error(format!(
            "demo expected {} tool results, got {}",
            expected.len(),
            results.len()
        )));
    }
    for (result, (call_id, tool)) in results.iter().zip(expected) {
        if result.call_id != *call_id || result.tool != *tool {
            return Err(script_error(format!(
                "demo expected the {tool} result for {call_id}"
            )));
        }
        if result.result.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(script_error(format!("demo tool {tool} did not succeed")));
        }
        if !demo_result_valid(tool, &result.result) {
            return Err(script_error(format!(
                "demo tool {tool} returned an unexpected result"
            )));
        }
    }
    Ok(())
}

fn demo_result_valid(tool: &str, result: &Value) -> bool {
    match tool {
        "write_file" => {
            result.get("bytes_written").and_then(Value::as_u64) == Some(DEMO_CONTENT.len() as u64)
        }
        "read_file" => result.get("content").and_then(Value::as_str) == Some(DEMO_CONTENT),
        "list_dir" => result
            .get("listing")
            .and_then(Value::as_str)
            .is_some_and(|listing| listing.lines().any(|line| line.ends_with("demo.txt"))),
        "workspace_diff" => result
            .get("added")
            .and_then(Value::as_array)
            .is_some_and(|added| added.iter().any(|path| path == "demo.txt")),
        "exec" => result
            .get("stdout")
            .and_then(Value::as_str)
            .is_some_and(|stdout| stdout.trim() == "24 /workspace/demo.txt"),
        _ => false,
    }
}
