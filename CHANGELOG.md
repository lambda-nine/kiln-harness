# Changelog

Notable changes to kiln-harness. Versions follow
[Semantic Versioning](https://semver.org/); before 1.0, any minor release may
change the CLI or audit format.

## Unreleased

- Third review round for the anthropic provider: a `fallback_message`
  iteration must be the only one and the last, after `message` attempts;
  content made only of `fallback` boundary blocks may carry zero output
  tokens, so a final fallback that declined before output reaches the
  engine as a refusal (the blocks still count as bytes, and every other
  block still needs a token); and strict-mode `enum` and `const` members
  must be strings, integers, booleans, or null. The strict subset is
  documented as deliberately narrow.
- The live API sends `usage.iterations` on every response when fallbacks
  are on, with `"model": null` for an attempt on the requested model, which
  the second-round validation refused. A `message` iteration may now leave
  `model` null or absent, meaning the requested model; a `fallback_message`
  must still name one; and a terminal entry without a model matches only
  when the response's `model` is the requested one. Unknown usage fields
  (`cache_creation`, `output_tokens_details`, `service_tier`, ...) are
  ignored. `anthropic::parse_response` takes the requested model, and the
  captured live usage is a test fixture that every non-fallback test now
  uses.
- Second review round for the anthropic provider. Top-level usage is
  always validated, and with `usage.iterations` every entry needs a known
  type, a valid model, valid counters, and nonzero input, and the last must
  equal the serving model and top-level counters exactly; the output floor
  runs on the serving attempt. Every error passes a final scan of its
  finished message for the key and is emptied on a match, keeping its kind,
  and keys under 20 bytes are refused as truncated. A `tool_use` stop must
  come with `tool_use` blocks and `end_turn` without them; the engine and
  grammar now treat any `tool_use` stop with no calls as a protocol fault
  (`InvalidModelCall`), which also covers openai-compat's
  `finish_reason: "tool_calls"` with no calls. The strict-tools transform
  keeps supported keywords and `minItems` 0 or 1, removes only size and
  range bounds the typed tools enforce, and refuses anything else, such as
  `pattern`, `multipleOf`, or `$ref`, with its path.
- Review fixes for the anthropic provider. Usage after a server-side
  fallback is the sum of every attempt in `usage.iterations`, each
  validated, so declined attempts count against all token budgets, and the
  per-request output cap now covers one call with its attempts
  (`--no-fallbacks` for a per-attempt cap). Authenticated error messages
  drop the error type and request ID when either contains the key. Opaque
  content holding a float or an integer outside 64 bits anywhere,
  `tool_use` inputs included, is refused, since it could not be echoed
  exactly. Responses must be a `message` from the
  `assistant` with a valid, now required, `model` and a known stop reason;
  `pause_turn`, unknown, and missing stop reasons are `InvalidResponse`,
  and the adapter no longer yields `StopReason::Other`. `--strict-tools`
  sends schemas without the keywords strict mode refuses and with
  `"required": []` where absent, after the live API refused `maxItems`;
  the canonical schemas and local validation are unchanged.
- Anthropic credential-proxy mode: `AnthropicConfig::api_key` is optional,
  and `run --provider anthropic` no longer requires `--api-key-env`.
  Without a key no `x-api-key` or `Authorization` header is sent, a
  host-side proxy adds the key, the harness skips the credential scan (the
  proxy owns it), and errors are quoted as bounded, sanitized excerpts as
  for an unauthenticated local server. Keyless use is accepted over plain
  `http` to any host and refused when the base URL is missing or is
  `api.anthropic.com`, with "needs --api-key-env, or --base-url pointing at
  a credential-injecting proxy". With a key, nothing changes.
- Add an `anthropic` provider for Anthropic's Messages API over raw HTTP
  (`anthropic-version: 2026-10-21`; no SDK, no new dependency), and
  `run --provider anthropic [--base-url] [--model] [--effort]
  [--no-fallbacks] [--strict-tools] [--api-key-env VAR]`. It defaults to
  `claude-opus-5-5` at high effort, non-streaming, with top-level automatic
  prompt caching and server-side fallbacks (`fallbacks: "default"`); it
  never sends sampling parameters, `thinking`, or `stream`. Each turn's tool
  results go in one user message. Errors quote only the status, an
  allowlisted error type, and a validated request ID. `--temperature` and
  `--seed` are refused with it, and `--effort`, `--strict-tools`, and
  `--no-fallbacks` with openai-compat; `--base-url` and `--model` become
  optional flags that openai-compat still requires.
- Add opaque provider content to the turn model: `ModelResponse` and
  `Message::Assistant` carry an optional `ProviderContent` (provider,
  served model, the response's content blocks as received, stop details)
  that the engine and the journal grammar put back into the history
  unchanged and the adapter that produced it echoes verbatim, as preserved
  thinking requires. It is skipped when absent, so earlier journals,
  request hashes, and openai-compat sessions are byte-identical; a recorded
  openai-compat journal is kept as a test fixture that must still verify,
  re-encode line for line, and replay. The audit's `model_responded`
  gains `served_model` when a provider reports one.
- Split the byte measures: `ModelResponse::output_bytes` (the session byte
  budget and the grammar) now also counts opaque content, and the new
  `token_floor_bytes` (adapters' 64-bytes-per-token usage floor) counts
  only the text and tool calls, since thinking signatures are unrelated to
  completion tokens. Zero output tokens for opaque content alone is still
  invalid usage. The credential scan covers opaque content.
- Factor the HTTP layer the adapters share (locked-down client, bounded
  bodies and error excerpts, seeded retries with capped `Retry-After`, the
  credential scan, the https-unless-loopback rule) into
  `src/providers/http.rs`, with openai-compat's behavior unchanged.
- Add the first network provider, `openai-compat`, for the OpenAI-compatible
  `/v1/chat/completions` API (MLX server, Ollama, llama.cpp, vLLM), and a
  `run` command that carries out a real task with it. Requests and responses
  map onto the harness's turn model; usage is normalized to the disjoint
  counters and a response without usage fails the session; request and
  response bodies are bounded; rate limits, overload, and transport failures
  are retried with seeded full-jitter backoff that honors `Retry-After`. An
  API key comes only from an environment variable named by `--api-key-env`,
  stays out of the journal, audit, and errors, and requires `https` unless
  the host is a loopback address. Journals from `run` replay without the
  model server. `reqwest` (rustls with ring, no native TLS) becomes a
  dependency.
- `read_file`, `write_file`, and `list_dir` also accept paths under
  `/workspace/` (and `list_dir` accepts `/workspace` itself), stripping the
  prefix once; other absolute paths stay `invalid_path`. The default system
  prompt says executions start in `/` and asks the model to report only what
  tool results showed. `run` defaults to 16,384 output tokens per request and
  600,000 per session, since hidden reasoning counts as output. Journals that
  recorded a refused `/workspace/...` path now diverge on replay, because the
  same call succeeds.
- Live smoke (Nov 10), Ollama serving `qwen3:4b` to `run` in the VM: with the
  old 4,096-token cap, the model's hidden reasoning filled the first turn and
  the session ended truncated; with a larger cap it answered from memory
  without calling a tool; with a prompt naming the tools it ran three turns
  and four calls, tripped on a `/workspace/`-prefixed path and on `exec`
  starting in `/`, and its journal verified and replayed as `reproduced` with
  the model server stopped.
- Review fixes for the provider: with an API key, error bodies are never
  quoted and a response containing the exact key (raw, JSON-escaped, or in
  parsed arguments and object keys) is refused, so the harness never writes
  the key or its JSON-escaped form anywhere; a transformed or split echo
  cannot be detected, and the docs now say so instead of promising
  redaction. Zero prompt tokens, zero completion tokens for non-empty
  output, and more than 64 output bytes per completion token are invalid
  usage. A new provider-neutral budget, `max_total_model_output_bytes`
  (`--max-total-output-bytes`, 8 MiB by default, 256 MiB ceiling), bounds
  model output locally, is recorded in `SessionLimits` (journals without it
  load with the default), and is applied by the journal grammar too. The
  response cap is documented as counting decoded payload bytes, with
  framing bounded by the deadline. `--prompt-file` and `--system-file` are
  opened non-blocking without following a final symlink and must be regular
  files. Final answers are printed with control characters escaped.
- Second review round: the credential scan also decodes the JSON nested in
  `function.arguments` when it is not an object, and looks for the key's
  JSON-escaped spelling as well as the key at every layer.
  `ModelResponse::output_bytes` now counts call IDs and tool names as well
  as text and arguments, and tool names over 256 bytes are refused. The
  grammar counts the same way, so a journal recorded under the earlier
  definition can now fail verification if its names and IDs push it past
  its byte budget; this is accepted because the format is unreleased.
- `HarnessConfig::validate` exposes the engine's limit checks so a command
  can refuse a bad configuration before opening outputs or contacting kilnd.
- A command that refuses its outputs, or cannot reach kilnd, now discards the
  journal before the audit, so a new directory the audit created for both is
  removed as well.

- Replace the one-action provider trait with `Provider::complete`: a
  `ModelRequest` (system, tools, message history, output-token cap) answered by
  a `ModelResponse` with text, any number of tool calls, a stop reason, and
  usage. Add per-turn call and token budgets, fail closed on truncated, refused,
  or empty responses, and turn unknown tool names into a model-visible
  `unknown_tool` result. Audit call IDs become `call-<turn>-<index>`.
- Add the hash-chained session journal (`journal` module, `--journal`), offline
  `journal verify`, and deterministic `replay` against kilnd.
- `list_dir` returns names only (`ls -1Ap`) so listings are stable across
  sessions and replays.
- Enable serde_json's `float_roundtrip` so journaled floats hash identically
  after a round trip.
- Harden the journal after review: one event grammar shared by writers,
  readers, and replay; every model-visible limit recorded, with hard ceilings
  on all limits; cached input counted against the input budget; request hashes
  that cover the output-token cap; `cleanup_failed` and `work_outcome` in
  terminal records; cancellation during create and the baseline; journals
  created only in operator-owned directories; and replay refusing outputs that
  alias its source.
- The VM integration check compares residue against a pre-run snapshot of
  node reservations and non-deleted sandboxes instead of zero.
- Second review round: the grammar binds tool calls, results, and request
  hashes; replay diverges at the first tool result that differs; unconfirmed
  create recovery ends `cleanup_failed`; journal directories are walked
  through pinned descriptors and sources opened non-blocking; replay compares
  outputs with its source by open descriptor; and verification checks recorded
  limits against the hard ceilings. `rustix` becomes a direct Unix
  dependency (already locked through `tempfile`).
- Third review round: the grammar enforces the engine's acceptance limits
  (steps, call budgets, token budgets, refused responses); interrupted tools
  reach `Provider::tool_completed` and replay counts unexpected calls;
  durations must be whole milliseconds; output paths must name a file; the
  audit uses the journal's directory walk and policy; unreadable ancestors are
  refused by name; failed or refused opens roll back what they created through
  pinned descriptors; and clippy is clean for Linux targets. On macOS,
  extended ACLs are not inspected, which is documented.
- Fourth review round: rollback stays complete when descriptors run out
  part-way through a deep path; the grammar applies the engine's task checks;
  replay keeps an unanswered recorded request as a turn to reach and locates a
  request past the end of the recording.
- Journals written before these review fixes do not load. `JOURNAL_SCHEMA`
  stays 1 because the journal format has not been released.
- Prepare the repository for public visibility: add the Apache-2.0 `LICENSE`
  (matching kiln and kilnd), `SECURITY.md` with private vulnerability
  reporting and scope, this changelog, and a README opening for new readers
  (status, the four Lambda IX repositories, a quickstart). Ignore `dist/` and
  `__pycache__/`.
