use std::collections::VecDeque;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelAction {
    ToolCall {
        call_id: String,
        tool: String,
        arguments: Value,
    },
    FinalAnswer {
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptItem {
    Model(ModelAction),
    ToolResult {
        call_id: String,
        tool: String,
        result: Value,
    },
}

pub struct ProviderContext<'a> {
    pub session_id: &'a str,
    pub turn: u32,
    pub tools: &'a [ToolDefinition],
    pub transcript: &'a [TranscriptItem],
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("provider failed: {message}")]
pub struct ProviderError {
    pub message: String,
}

#[async_trait]
pub trait Provider: Send {
    async fn next(&mut self, context: ProviderContext<'_>) -> Result<ModelAction, ProviderError>;
}

/// A deterministic provider used by tests and the no-API demo.
pub struct ScriptedProvider {
    actions: VecDeque<ModelAction>,
    validate_demo_results: bool,
}

impl ScriptedProvider {
    #[must_use]
    pub fn new(actions: impl IntoIterator<Item = ModelAction>) -> Self {
        Self {
            actions: actions.into_iter().collect(),
            validate_demo_results: false,
        }
    }

    #[must_use]
    pub fn demo() -> Self {
        let mut provider = Self::new([
            ModelAction::ToolCall {
                call_id: "call-1".to_string(),
                tool: "write_file".to_string(),
                arguments: json!({
                    "path": "demo.txt",
                    "content": "hello from kiln-harness\n"
                }),
            },
            ModelAction::ToolCall {
                call_id: "call-2".to_string(),
                tool: "read_file".to_string(),
                arguments: json!({"path": "demo.txt", "max_bytes": 4096}),
            },
            ModelAction::ToolCall {
                call_id: "call-3".to_string(),
                tool: "list_dir".to_string(),
                arguments: json!({"path": "."}),
            },
            ModelAction::ToolCall {
                call_id: "call-4".to_string(),
                tool: "workspace_diff".to_string(),
                arguments: json!({}),
            },
            ModelAction::ToolCall {
                call_id: "call-5".to_string(),
                tool: "exec".to_string(),
                arguments: json!({
                    "argv": ["/usr/bin/wc", "-c", "/workspace/demo.txt"],
                    "stdin": ""
                }),
            },
            ModelAction::FinalAnswer {
                text: "The deterministic harness wrote, read, listed, diffed, and counted demo.txt through kilnd.".to_string(),
            },
        ]);
        provider.validate_demo_results = true;
        provider
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn next(&mut self, context: ProviderContext<'_>) -> Result<ModelAction, ProviderError> {
        if context.tools.is_empty() {
            return Err(ProviderError {
                message: "tool registry is empty".to_string(),
            });
        }
        if self.validate_demo_results {
            validate_demo_result(&context)?;
        }
        self.actions.pop_front().ok_or_else(|| ProviderError {
            message: "script ended before a final answer".to_string(),
        })
    }
}

fn validate_demo_result(context: &ProviderContext<'_>) -> Result<(), ProviderError> {
    if context.turn == 1 {
        return Ok(());
    }
    let Some(TranscriptItem::ToolResult { tool, result, .. }) = context.transcript.last() else {
        return Err(ProviderError {
            message: "demo expected a tool result before the next turn".to_string(),
        });
    };
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(ProviderError {
            message: format!("demo tool {tool} did not succeed"),
        });
    }
    let valid = match context.turn {
        2 => {
            tool == "write_file" && result.get("bytes_written").and_then(Value::as_u64) == Some(24)
        }
        3 => {
            tool == "read_file"
                && result.get("content").and_then(Value::as_str)
                    == Some("hello from kiln-harness\n")
        }
        4 => {
            tool == "list_dir"
                && result
                    .get("listing")
                    .and_then(Value::as_str)
                    .is_some_and(|listing| listing.contains("demo.txt"))
        }
        5 => {
            tool == "workspace_diff"
                && result
                    .get("added")
                    .and_then(Value::as_array)
                    .is_some_and(|added| added.iter().any(|path| path == "demo.txt"))
        }
        6 => {
            tool == "exec"
                && result
                    .get("stdout")
                    .and_then(Value::as_str)
                    .is_some_and(|stdout| stdout.trim() == "24 /workspace/demo.txt")
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(ProviderError {
            message: format!("demo tool {tool} returned an unexpected result"),
        })
    }
}
