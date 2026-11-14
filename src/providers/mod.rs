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

use crate::tools::{ToolLimits, ToolSet};

pub mod anthropic;
pub mod http;
pub mod openai_compat;

pub use anthropic::{AnthropicConfig, AnthropicError, AnthropicProvider, Effort};
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

Commands run with working directory /workspace, so relative paths refer to \
workspace files, for example [\"/bin/sh\", \"fib.sh\"]; the file tools accept \
fib.sh or /workspace/fib.sh.

Every result is JSON and at most {result_kib} KiB; longer output is truncated. \
Use the tools to carry out the task. When it is done, answer in plain text \
with no tool calls, and report only what tool results showed.";

/// [`DEFAULT_SYSTEM_PROMPT`] with its placeholders filled from `limits`.
#[must_use]
pub fn default_system_prompt(limits: &ToolLimits) -> String {
    fill_prompt(DEFAULT_SYSTEM_PROMPT, limits)
}

/// The default system prompt for a session with `tool_set`: for
/// [`ToolSet::Base`] exactly [`default_system_prompt`]; for
/// [`ToolSet::CodingV1`] the same template with the coding tools listed,
/// then a paragraph on when to prefer them.
#[must_use]
pub fn default_system_prompt_for(limits: &ToolLimits, tool_set: ToolSet) -> String {
    match tool_set {
        ToolSet::Base => default_system_prompt(limits),
        ToolSet::CodingV1 => fill_prompt(&coding_template(), limits),
    }
}

/// The base tool lines the coding template changes, and what it puts in
/// their place.
const CODING_PROMPT_EDITS: [(&str, &str); 3] = [
    (
        "- read_file: read up to {file_kib} KiB of a file under /workspace.\n",
        "- read_file: read up to {file_kib} KiB of a file under /workspace, or whole lines from \
         start_line (with line_count) within its first 1 MiB.\n",
    ),
    (
        "- list_dir: ",
        "- edit_file: replace exact text in a file of at most {file_kib} KiB; each old_text \
         must match once unless replace_all is set, and all edits apply or none.\n\
         - search: find lines containing a string (or an extended regex) in the files under a \
         /workspace path; results are path:line:text.\n\
         - list_dir: ",
    ),
    (
        "- workspace_diff: list files added or removed since the session started.\n",
        "- workspace_diff: list files added or removed since the session started.\n\n\
         Prefer edit_file over write_file to change part of an existing file, and search or a \
         line range over reading whole files.\n",
    ),
];

/// [`DEFAULT_SYSTEM_PROMPT`] with [`CODING_PROMPT_EDITS`] applied.
fn coding_template() -> String {
    CODING_PROMPT_EDITS
        .iter()
        .fold(DEFAULT_SYSTEM_PROMPT.to_string(), |template, (from, to)| {
            debug_assert_eq!(template.matches(from).count(), 1, "{from}");
            template.replacen(from, to, 1)
        })
}

fn fill_prompt(template: &str, limits: &ToolLimits) -> String {
    template
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
        assert!(prompt.contains("working directory /workspace"), "{prompt}");
        assert!(
            prompt.contains("report only what tool results showed"),
            "{prompt}"
        );
        assert!(prompt.len() < 2048);
    }

    #[test]
    fn the_coding_prompt_lists_the_coding_tools_and_keeps_the_rest() {
        let limits = ToolLimits::default();
        assert_eq!(
            default_system_prompt_for(&limits, ToolSet::Base),
            default_system_prompt(&limits)
        );
        let base = default_system_prompt(&limits);
        let coding = default_system_prompt_for(&limits, ToolSet::CodingV1);
        assert!(!coding.contains('{'), "{coding}");
        let tools = |prompt: &str| {
            prompt
                .lines()
                .filter_map(|line| line.strip_prefix("- "))
                .filter_map(|line| line.split_once(':').map(|(name, _)| name.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            tools(&coding),
            [
                "exec",
                "read_file",
                "write_file",
                "edit_file",
                "search",
                "list_dir",
                "workspace_diff"
            ]
        );
        // Every base line but read_file's is unchanged, and the rest of the
        // template too.
        for line in base.lines().filter(|line| !line.starts_with("- read_file")) {
            assert!(coding.lines().any(|coding| coding == line), "{line}");
        }
        assert!(coding.contains(
            "- read_file: read up to 64 KiB of a file under /workspace, or whole lines from \
             start_line (with line_count) within its first 1 MiB.\n"
        ));
        assert!(coding.contains(
            "since the session started.\n\nPrefer edit_file over write_file to change part of \
             an existing file, and search or a line range over reading whole files.\n\nCommands"
        ));
        assert!(coding.contains("at most 64 KiB; each old_text"));
        assert!(coding.len() < 3072);
    }
}
