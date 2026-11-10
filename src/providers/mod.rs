//! Network model providers.
//!
//! Each adapter translates the harness's provider-neutral [`ModelRequest`]
//! and [`ModelResponse`] to one wire API and owns that API's retry policy;
//! the engine treats every error an adapter returns as fatal. Adapters bound
//! what they read from the network before parsing it, normalize usage to the
//! disjoint contract in [`crate::provider::Usage`], and keep credentials out
//! of requests, responses, and error messages, so nothing secret reaches the
//! journal or the audit.
//!
//! [`ModelRequest`]: crate::provider::ModelRequest
//! [`ModelResponse`]: crate::provider::ModelResponse

use std::fmt;
use std::time::Duration;

use crate::tools::ToolLimits;

pub mod openai_compat;

pub use openai_compat::{OpenAiCompatConfig, OpenAiCompatError, OpenAiCompatProvider};

/// A credential that must never be printed, serialized, or journaled.
///
/// Its `Debug` output is redacted and it implements neither `Display` nor
/// `Serialize`. Adapters read it only to build a sensitive request header.
#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([redacted])")
    }
}

// TODO(kilnd M3): once kilnd's exec working directory ships, set it to
// /workspace for every execution and change the sentence about working
// directory / below.
/// The system prompt `run` uses when no `--system-file` is given. `{...}`
/// placeholders are filled from the session's tool limits by
/// [`default_system_prompt`], so the model is told the limits that apply.
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You work in one Linux sandbox through tools. The sandbox has no network \
access. /workspace is writable and starts empty; the root filesystem is \
read-only. No host shell is involved: commands run as literal argv inside \
the sandbox.

Tools:
- exec: run argv (1 to 128 strings) with optional stdin, for example \
[\"/bin/sh\", \"-c\", \"ls -la\"]. Each call is limited to {timeout_secs} s.
- read_file: read up to {file_kib} KiB of a file under /workspace.
- write_file: replace a file under /workspace with at most {file_kib} KiB of \
UTF-8 text.
- list_dir: list entry names in a directory under /workspace.
- workspace_diff: list files added or removed since the session started.

Commands run with working directory /, so refer to workspace files by \
absolute path, for example [\"/bin/sh\", \"/workspace/fib.sh\"]; the file \
tools accept fib.sh or /workspace/fib.sh.

Every result is JSON and at most {result_kib} KiB; longer output is truncated. \
Use the tools to carry out the task. When it is done, answer in plain text \
with no tool calls, and report only what tool results showed.";

/// [`DEFAULT_SYSTEM_PROMPT`] with its placeholders filled from `limits`.
#[must_use]
pub fn default_system_prompt(limits: &ToolLimits) -> String {
    DEFAULT_SYSTEM_PROMPT
        .replace("{timeout_secs}", &format_secs(limits.timeout))
        .replace("{file_kib}", &(limits.max_file_bytes / 1024).to_string())
        .replace(
            "{result_kib}",
            &(limits.max_result_bytes / 1024).to_string(),
        )
}

fn format_secs(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis % 1000 == 0 {
        (millis / 1000).to_string()
    } else {
        format!("{:.3}", duration.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_is_redacted() {
        let secret = SecretString::new("sk-very-secret");
        assert_eq!(format!("{secret:?}"), "SecretString([redacted])");
        assert_eq!(secret.expose(), "sk-very-secret");
    }

    #[test]
    fn default_prompt_states_the_configured_limits() {
        let limits = ToolLimits {
            timeout: Duration::from_secs(30),
            ..ToolLimits::default()
        };
        let prompt = default_system_prompt(&limits);
        assert!(prompt.contains("limited to 30 s"), "{prompt}");
        assert!(prompt.contains("64 KiB"), "{prompt}");
        assert!(prompt.contains("128 KiB"), "{prompt}");
        assert!(!prompt.contains('{'), "{prompt}");
        assert!(prompt.contains("working directory /"), "{prompt}");
        assert!(
            prompt.contains("report only what tool results showed"),
            "{prompt}"
        );
        assert!(prompt.len() < 2048);
    }
}
