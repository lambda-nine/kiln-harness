# Changelog

Notable changes to kiln-harness. Versions follow
[Semantic Versioning](https://semver.org/); before 1.0, any minor release may
change the CLI or audit format.

## Unreleased

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
