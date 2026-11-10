# Changelog

Notable changes to kiln-harness. Versions follow
[Semantic Versioning](https://semver.org/); before 1.0, any minor release may
change the CLI or audit format.

## Unreleased

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

- Prepare the repository for public visibility: add the Apache-2.0 `LICENSE`
  (matching kiln and kilnd), `SECURITY.md` with private vulnerability
  reporting and scope, this changelog, and a README opening for new readers
  (status, the four Lambda IX repositories, a quickstart). Ignore `dist/` and
  `__pycache__/`.
