//! Recorded context compaction (`--compact-at`).
//!
//! A long session can outgrow its prompt. With a [`CompactionPolicy`], the
//! engine asks the provider, at the top of a turn, to summarize the whole
//! history once [`compaction_due`] says the next prompt would pass the
//! policy's threshold. On success the history becomes the provider's signed
//! summary followed by the task restated ([`swapped_history`]), and the
//! session goes on from there. Every attempt is journaled, priced and
//! counted against the session's budgets, and replay answers it from the
//! recording without contacting a model.
//!
//! The threshold is a heuristic estimate from the previous response's
//! reported usage, not a guarantee: it does not keep any request under a
//! context window or a price line.

use serde::{Deserialize, Serialize};

use crate::provider::{CompactionResponse, Message, ProviderContent, Usage};

/// The beta that enables on-demand compaction, sent on the compaction
/// request and on every later request that carries the summary.
pub const COMPACTION_BETA: &str = "compact-2026-09-04";
/// The provider whose summaries the grammar accepts: the only adapter that
/// compacts.
pub const COMPACTION_PROVIDER: &str = "anthropic";
/// The `stop_reason` of a response that holds a summary.
pub const COMPACTION_STOP: &str = "compaction";
/// The type of the one content block a summary arrives in.
pub const COMPACTION_BLOCK: &str = "compaction";

/// Host text sent as the summarizer's `instructions`. Versioned by its
/// SHA-256 in [`CompactionPolicy::instructions_sha256`].
pub const COMPACTION_INSTRUCTIONS: &str = "\
Summarize this agent session so the work can continue from your summary alone: \
it replaces the whole conversation. Keep everything still needed:\n\
1. The task and every constraint on it, as given.\n\
2. Every file created or changed, with its path and its purpose.\n\
3. The commands run and their decisive results, errors included.\n\
4. The current state of the work and the remaining plan, with the next step.\n\
Write the summary only. Do not call any tools.";

/// The text before the restated task in the user turn that follows a
/// summary.
pub const RESUME_NOTE: &str = "The conversation so far is summarized above. Continue the task \
from where it stopped. The task, as first given:\n\n";

/// Successful compactions per session.
pub const MAX_COMPACTIONS: u32 = 16;
/// Compaction attempts that returned no summary, per session.
pub const MAX_NO_SUMMARY: u32 = 2;
/// The smallest `--compact-at`.
pub const MIN_COMPACT_AT: u64 = 20_000;
/// The largest `--compact-at`.
pub const MAX_COMPACT_AT: u64 = 1_000_000;
/// Room kept below a listed model's context window above the threshold
/// and the output limit: the tool results appended after the estimate and
/// the summarizer's own prompt.
pub const COMPACTION_HEADROOM_TOKENS: u64 = 32_000;
/// The longest `stop_reason` a compaction response may record.
pub const MAX_STOP_REASON_BYTES: usize = 64;

/// When and how often a session compacts, recorded in `session_started` so
/// `journal verify` and replay use the recorded policy, never flags.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompactionPolicy {
    /// Compact when the next prompt's estimate is over this many tokens.
    pub compact_at: u64,
    /// Successful compactions allowed per session.
    pub max_compactions: u32,
    /// Attempts without a summary allowed per session.
    pub max_no_summary: u32,
    /// SHA-256 hex of the [`COMPACTION_INSTRUCTIONS`] the session sent.
    pub instructions_sha256: String,
}

impl CompactionPolicy {
    /// The policy this build runs for `--compact-at TOKENS`.
    #[must_use]
    pub fn new(compact_at: u64) -> Self {
        Self {
            compact_at,
            max_compactions: MAX_COMPACTIONS,
            max_no_summary: MAX_NO_SUMMARY,
            instructions_sha256: instructions_sha256(),
        }
    }

    /// Why the policy could not have come from any build, if it could not:
    /// a threshold outside [`MIN_COMPACT_AT`]..=[`MAX_COMPACT_AT`], caps
    /// outside 1 and this build's ceilings, or a hash that is not 64
    /// lower-case hex digits. Instructions from another build pass.
    #[must_use]
    pub fn problem(&self) -> Option<&'static str> {
        if !(MIN_COMPACT_AT..=MAX_COMPACT_AT).contains(&self.compact_at) {
            return Some("compact_at is outside 20000..=1000000");
        }
        if !(1..=MAX_COMPACTIONS).contains(&self.max_compactions) {
            return Some("max_compactions is outside 1..=16");
        }
        if !(1..=MAX_NO_SUMMARY).contains(&self.max_no_summary) {
            return Some("max_no_summary is outside 1..=2");
        }
        let hex = self.instructions_sha256.len() == 64
            && self
                .instructions_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !hex {
            return Some("instructions_sha256 is not a SHA-256 hex digest");
        }
        None
    }

    /// Whether the policy's instructions are this build's
    /// [`COMPACTION_INSTRUCTIONS`]: replay needs them to be.
    #[must_use]
    pub fn instructions_are_current(&self) -> bool {
        self.instructions_sha256 == instructions_sha256()
    }
}

/// SHA-256 hex of [`COMPACTION_INSTRUCTIONS`].
#[must_use]
pub fn instructions_sha256() -> String {
    crate::journal::sha256_hex(COMPACTION_INSTRUCTIONS.as_bytes())
}

/// Whether to compact before the next request. Deterministic from
/// journaled facts; the engine and the grammar both call it.
///
/// `previous` is the usage of the last `model_responded`, and
/// `appended_bytes` the compact JSON bytes of what was pushed after it (the
/// tool-results message or the truncation note, see [`message_bytes`]),
/// counted at a conservative 3 bytes per token. `successes` and
/// `no_summaries` are the session's compactions so far.
#[must_use]
pub fn compaction_due(
    policy: &CompactionPolicy,
    previous: &Usage,
    appended_bytes: u64,
    successes: u32,
    no_summaries: u32,
) -> bool {
    successes < policy.max_compactions
        && no_summaries < policy.max_no_summary
        && previous
            .total_input_tokens()
            .saturating_add(previous.output_tokens)
            .saturating_add(appended_bytes.div_ceil(3))
            > policy.compact_at
}

/// Compact JSON bytes of one neutral message, saturating.
#[must_use]
pub fn message_bytes(message: &Message) -> u64 {
    let mut count = crate::provider::ByteCount(0);
    // Encoding a message into a counter cannot fail.
    let _ = serde_json::to_writer(&mut count, message);
    count.0
}

/// The user turn that follows a summary: [`RESUME_NOTE`] and the task.
#[must_use]
pub fn resume_text(prompt: &str) -> String {
    format!("{RESUME_NOTE}{prompt}")
}

/// The history after a successful compaction: the summary, then the task
/// restated.
#[must_use]
pub fn swapped_history(
    summary: String,
    provider_content: ProviderContent,
    prompt: &str,
) -> Vec<Message> {
    vec![
        Message::Compacted {
            summary,
            provider_content,
        },
        Message::User {
            text: resume_text(prompt),
        },
    ]
}

/// Whether `stop_reason` is one a compaction response may record: 1 to
/// [`MAX_STOP_REASON_BYTES`] bytes of lower-case letters, digits and `_`.
#[must_use]
pub fn stop_reason_is_valid(stop_reason: &str) -> bool {
    !stop_reason.is_empty()
        && stop_reason.len() <= MAX_STOP_REASON_BYTES
        && stop_reason
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Why a compaction response could not be what the engine journals, if it
/// could not. The engine and the grammar share it.
///
/// A summary (`stop_reason` [`COMPACTION_STOP`]) needs a non-empty
/// `summary` and [`COMPACTION_PROVIDER`] content whose blocks are exactly
/// one [`COMPACTION_BLOCK`] block with a string `content` equal to the
/// summary and a string `signature`. Any other valid stop reason needs
/// neither. The signature is not, and cannot be, authenticated offline.
#[must_use]
pub fn response_problem(response: &CompactionResponse) -> Option<&'static str> {
    if !stop_reason_is_valid(&response.stop_reason) {
        return Some("compaction stop_reason is not a valid stop reason");
    }
    if response.stop_reason != COMPACTION_STOP {
        if response.summary.is_some() || response.provider_content.is_some() {
            return Some("a compaction response without a summary carries content");
        }
        return None;
    }
    let (Some(summary), Some(content)) = (&response.summary, &response.provider_content) else {
        return Some("a compaction stop needs a summary and its provider content");
    };
    if summary.is_empty() {
        return Some("a compaction summary is empty");
    }
    if content.provider != COMPACTION_PROVIDER {
        return Some("a compaction summary is not from anthropic");
    }
    if content.stop_details.is_some() {
        return Some("a compaction summary carries stop_details");
    }
    let Some([block]) = content.blocks.as_array().map(Vec::as_slice) else {
        return Some("a compaction summary is not exactly one content block");
    };
    let field = |name: &str| block.get(name).and_then(serde_json::Value::as_str);
    if field("type") != Some(COMPACTION_BLOCK) {
        return Some("a compaction summary's block is not a compaction block");
    }
    if field("content") != Some(summary.as_str()) {
        return Some("a compaction summary differs from its block's content");
    }
    if field("signature").is_none() {
        return Some("a compaction summary's block has no string signature");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_fit_in_two_kib_and_hash_stably() {
        assert!(COMPACTION_INSTRUCTIONS.len() < 2048);
        let policy = CompactionPolicy::new(80_000);
        assert_eq!(policy.problem(), None);
        assert!(policy.instructions_are_current());
        assert_eq!(policy.instructions_sha256, instructions_sha256());
    }

    #[test]
    fn due_counts_usage_output_and_appended_bytes() {
        let policy = CompactionPolicy::new(20_000);
        let usage = Usage {
            input_tokens: 10_000,
            output_tokens: 5_000,
            cache_read_input_tokens: 4_000,
            cache_creation_input_tokens: 1_000,
        };
        assert!(!compaction_due(&policy, &usage, 0, 0, 0), "exactly at");
        assert!(compaction_due(&policy, &usage, 1, 0, 0), "one byte over");
        assert!(!compaction_due(&policy, &usage, 1, 16, 0), "success cap");
        assert!(!compaction_due(&policy, &usage, 1, 0, 2), "no-summary cap");
    }

    #[test]
    fn stop_reasons_are_bounded() {
        assert!(stop_reason_is_valid("max_tokens"));
        assert!(!stop_reason_is_valid(""));
        assert!(!stop_reason_is_valid("End Turn"));
        assert!(!stop_reason_is_valid(&"a".repeat(65)));
    }
}
