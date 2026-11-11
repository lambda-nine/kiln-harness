# Changelog

Notable changes to kiln-harness. Versions follow
[Semantic Versioning](https://semver.org/); before 1.0, any minor release may
change the CLI or audit format.

## Unreleased

- Seventh review round for kilnd API milestone 1. An unreadable recovery
  window with no rows now stops the session as documented, rather than
  being dropped for lack of IDs. Cancellation and the session deadline win
  over the final check before it starts, while it runs (cancellation is
  polled first), and after it answers, so a session canceled or past its
  deadline at the end is never recorded `completed`.
- Sixth review round for kilnd API milestone 1. A session can now be
  recorded `completed` only after a final check: one `GetSandbox`
  (`SandboxApi::final_check`) after the final answer must show the sandbox
  READY and every execution the session never saw finished and reusable.
  Otherwise the session fails with the mapped stop, `not usable`, or
  `could not confirm`, and the audit records a metadata-only
  `final_check` event; the journal format is unchanged. Response-loss
  recovery is best-effort: its last pass starts `min(100 ms, window / 4)`
  before its deadline, a failed last read with nothing found fails closed,
  and cancellation ends it at once. Every `GetSandbox` answer must name the
  sandbox asked about. A tool call's outer deadline is
  `min(session deadline, tool start + budget)`, and the docs now say the
  tool timeout bounds the call's Execs, with recovery after. A forced renew
  attempt is cut off at its give-up instant.
- Fifth review round for kilnd API milestone 1. Response-loss recovery
  reads until its deadline unless every row it found has settled, with its
  last passes just before and at the deadline, and fails closed when no
  `GetSandbox` read succeeded at all. A forced renew before a tool call must
  succeed before the tool starts: a transient failure is retried until the
  earlier of the session and estimated idle deadlines, then fails closed.
  Activity is anchored when its proving request was sent (create, Exec,
  renew), not when the reply arrived. The tool timeout now bounds a whole
  tool call, every Exec and its recovery included, so `read_file` cannot
  take two timeouts. `ToolRegistry::start_session` takes the time create
  was sent, and `ToolSession::record_activity` the time the request was.
- Replay picks its sandbox idle timeout itself: the one the recording sent,
  when it ran with expiry, and otherwise the smallest whole second of at
  least 120 that fits the recorded tool timeout, so a recording with long
  tools still replays against a daemon with expiry.
  `ReplayProvider::replay_config` builds the configuration, and
  `HarnessConfig::fit_replay_idle_timeout` makes the choice.
- Fourth review round for kilnd API milestone 1. With expiry, the tool
  timeout plus the response-loss recovery window plus the renew interval
  must fit in the idle timeout sent, or the session is refused before
  create, and a renew is forced before a kilnd-bound tool call that could
  otherwise reach the idle deadline, whatever the retry backoff. Recovery
  polling backs off from 10 ms to 200 ms and rereads `GetSandbox` only
  every fourth pass and once at the end. A tool call's `model_turn` audit
  record now precedes its pre-tool renew, and a session deadline that
  passes during that renew wins over `renew_failed` and `SandboxExpired`.
- Third review round for kilnd API milestone 1. Recovery after a lost Exec
  response takes only terminal rows as evidence: it follows each new row
  with `GetExecution` (new `SandboxApi::get_execution`) until it settles,
  and a row still running at the recovery deadline ends the session as an
  `Api` error, after the tool result is journaled. An overdue renew now runs
  before each kilnd-bound tool call as well as each provider call, through
  one `renew_if_due`. Failed renews are reset by any later activity,
  ordered by an activity generation rather than the clock. A
  `sandbox_not_ready` Exec refusal on a sandbox that reads READY (new
  `SandboxApi::sandbox_ready`), kilnd's retryable insert race, is an
  ordinary tool error. A renew answer must report the sandbox READY.
  `ToolStop::Unusable` carries its message, and tools are checked on the
  host before any kilnd RPC (`ToolSession::contacts_kilnd`).
- Second review round for kilnd API milestone 1. Evidence recovered after
  a lost Exec response keeps each row's `sandbox_reusable` and
  `cancel_reason`, and a recovered row that left the sandbox unusable ends
  the session as a returned one does, during the baseline too. A typed
  `sandbox_failed`, `sandbox_lost`, `sandbox_quarantined` or
  `sandbox_not_ready` refusal of an Exec ends the session after its
  `kilnd_error` result is journaled, and of a renew is terminal. The renew
  schedule counts only activity the harness can prove: a returned or
  recovered execution, not a failed Exec RPC. A `backend_unsupported:`
  reason must name a feature of `[a-z0-9_]` to make a create rejection
  definite.
- First review round for kilnd API milestone 1. An execution whose RPC
  succeeded but whose `sandbox_reusable` is false ends the session after its
  tool result is journaled: as `SandboxExpired` when its `cancel_reason` is
  a deletion or a lifetime expiry, and otherwise as an `Api` error, with no
  further execution or model call, during the baseline too; `Execution`
  keeps both fields. The renew schedule is session-wide, one interval after
  the last activity kilnd saw, so short execution-free turns cannot add up
  to the idle timeout, and an overdue renew runs before the next model call.
  `RESOURCE_EXHAUSTED` on renew is transient. The CLI's `connect` no longer
  calls Health, so the engine's bounded discovery is the only call before a
  session, and the check after it is bounded by the lifecycle window and
  only warns. Incoherent advertised expiry limits are refused before
  create. The journal grammar bounds `sandbox_ready`'s expiry: a lifetime
  over the recorded session timeout and within the hard ceilings, and an
  idle timeout from 10 seconds to an hour; the engine refuses before create
  any policy outside those bounds. A renew response must name the sandbox,
  and `backend_unsupported:` must name a feature to count as definite.
- The README pins two kiln commits: the tested one, `00d61c5`, which
  `scripts/integration-vm.sh` now requires as a clean build before it
  touches kilnd, and the minimum, `e48f0e7`, the oldest kiln with every
  feature kilnd `853dee2` requires, for reference only.
- kilnd API milestone 1 (kilnd `853dee2`, whose `kilnd.v1` schema is now
  vendored). Each session reads Health once before create and uses only
  the features a serving daemon lists; `session_opened` records them as
  `features`. With sandbox expiry, create sends an idle timeout
  (`--sandbox-idle-timeout-secs`, 120 by default) and a maximum lifetime
  (the session, its cleanup window, and `--sandbox-lifetime-margin-secs`,
  60 by default), clamped into the daemon's bounds, so kilnd reclaims a
  crashed harness's sandbox; a lifetime cap shorter than the session is
  refused before create. During each provider call a `RenewSandbox`
  heartbeat keeps the sandbox from idling out; `SessionOutcome` and
  `session_finished` count successful renews as `renews`. With typed
  reasons, an `expired_idle`, `expired_lifetime` or `sandbox_deleted`
  refusal from any sandbox call, or too many failed renews in a row, ends
  the session with `SandboxExpired` after the usual verified cleanup, and
  `UNIMPLEMENTED` with `backend_unsupported:*` is a definite create
  rejection. `sandbox_created` records the expiry sent, or `null` against
  an older daemon, which gets none and behaves as before. Journal
  `sandbox_ready` carries the expiry as optional fields, skipped when
  absent, so older journals are unchanged. `SandboxApi` gains `renew`,
  `SandboxRequest` and `Sandbox` an expiry, `Health` its capabilities,
  `ApiError` a validated `reason`, and `ToolRegistry::start_session` a
  `typed_reasons` flag.
- Sixth review round for the anthropic provider: without fallback
  evidence (no `fallback` block and no `fallback_message` iteration), the
  response's `model` and every named `message` iteration must equal the
  requested model exactly, with fallbacks off or on, so an unmarked model
  switch is refused before any tool runs. The captured fallbacks-off live
  usage (top level only, no `iterations`) is a second test fixture.
- Anthropic server-side fallbacks are now opt-in: `--fallbacks` replaces
  `--no-fallbacks`, and by default the request carries no `fallbacks` field
  and no beta header. The harness records, replays, and compares specific
  models, and a refusal silently rerouted to another model breaks that
  attribution; without fallbacks a refusal is an honest `ModelRefused`.
  With fallbacks off, any `fallback` block or `fallback_message` iteration
  is refused, and `usage.iterations`, when present, is validated like any
  other. Fifth review round, on the opt-in path: every `fallback` block
  needs its declined attempt as an ordinary `message` iteration, in order
  and with that block's `from.model`, before the one terminal
  `fallback_message`, so the array is exactly one entry per block plus one;
  and any block before the last `fallback` block, such as a declined
  model's tool call, is refused as content before the fallback boundary.
- Fourth review round for the anthropic provider: with fallbacks on,
  `usage.iterations` is required, as the live API always sends it; with
  fallbacks off it may be missing, and a `fallback_message`
  entry or a `fallback` block is refused. `fallback` blocks in the content
  must match the usage: the last iteration is a `fallback_message` whose
  model, and the response's, is the last block's `to.model`, and the first
  block's `from.model` is the requested model. `anthropic::parse_response`
  takes the fallback mode.
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
  (fallbacks off for a per-attempt cap). Authenticated error messages
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
  [--fallbacks] [--strict-tools] [--api-key-env VAR]`. It defaults to
  `claude-opus-5-5` at high effort, non-streaming, with top-level automatic
  prompt caching and no server-side fallbacks (`--fallbacks` sends
  `fallbacks: "default"`); it
  never sends sampling parameters, `thinking`, or `stream`. Each turn's tool
  results go in one user message. Errors quote only the status, an
  allowlisted error type, and a validated request ID. `--temperature` and
  `--seed` are refused with it, and `--effort`, `--strict-tools`, and
  `--fallbacks` with openai-compat; `--base-url` and `--model` become
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
