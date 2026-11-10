//! Usage shapes for the anthropic provider tests.

use serde_json::{Value, json};

/// `usage` exactly as `claude-opus-5-5` returned it live, with
/// `fallbacks: "default"` and its beta header, on a response where no
/// fallback ran.
pub const LIVE_USAGE: &str = include_str!("../fixtures/anthropic-live-usage.json");

/// `usage` exactly as `claude-opus-5-5` returned it live without
/// fallbacks: top-level counters only, no `iterations`.
pub const LIVE_USAGE_FALLBACKS_OFF: &str =
    include_str!("../fixtures/anthropic-live-usage-fallbacks-off.json");

/// [`LIVE_USAGE`]'s shape with other counters: the real non-fallback form,
/// one `message` iteration with a null model mirroring the top level, and
/// the extra fields the API adds. Prefer it to invented usage objects.
pub fn live_usage(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> Value {
    let cache =
        json!({"ephemeral_5m_input_tokens": cache_creation, "ephemeral_1h_input_tokens": 0});
    json!({
        "input_tokens": input,
        "cache_creation_input_tokens": cache_creation,
        "cache_read_input_tokens": cache_read,
        "cache_creation": cache,
        "iterations": [{
            "input_tokens": input,
            "output_tokens": output,
            "cache_read_input_tokens": cache_read,
            "cache_creation_input_tokens": cache_creation,
            "cache_creation": cache,
            "type": "message",
            "model": null
        }],
        "output_tokens": output,
        "output_tokens_details": {"thinking_tokens": 0},
        "service_tier": "standard",
        "inference_geo": "global",
        "speed": "standard"
    })
}
