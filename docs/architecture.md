# Architecture

```text
scripted or replay provider (live adapter later)
        │ ModelRequest ▲ ModelResponse (text, tool calls, stop reason, usage)
        ▼              │
agent loop ── steps/calls/tokens/deadline ──┬─ append-only audit
        │ typed ToolCall                    │    └─ hashes, sizes, IDs only
        ▼                                   └─ session journal
tool registry ── strict schemas and          └─ content, hash-chained
                 byte/time bounds
        │ literal argv via kilnd.v1
        ▼
kilnd Unix socket ── one sandbox ── bounded executions
```

`Provider` and `SandboxApi` are separate traits. The first answers one
`ModelRequest` (system prompt, tool definitions, message history, output-token
cap) with one `ModelResponse`: optional text, zero or more tool calls, a stop
reason, and token usage. The second represents only kilnd API semantics. The real
adapter uses gRPC over the configured Unix socket. Tests use an in-memory fake.
No harness code depends on kiln files, process layout, or CLI behavior.

## Session lifecycle

1. Validate the limits against their hard ceilings and the task (system and
   prompt at most 64 KiB each, nonempty prompt), calculate one absolute session deadline, open the audit
   session, and journal `session_started`.
2. Create exactly one sandbox with network none, 256 MiB memory, 0.5 CPU,
   64 PIDs, 256 MiB disk, read-only root, and writable `/workspace`.
3. Capture a bounded `/workspace` file-set baseline through kilnd and journal
   `sandbox_ready`.
4. Alternate model turns and their tool results until a final answer or a
   budget/cancellation/error ends work.
5. Retry idempotent `DeleteSandbox` and independently verify absence with a
   complete non-deleted listing after every acknowledgement or ambiguous error.
   Cleanup has a separate 10-second wall window because kilnd may need to drain
   descendants and publish terminal state after the work deadline. Failure to
   confirm deletion is returned as a cleanup error.
6. Write audit `session_finished` and journal `session_ended` with the outcome,
   the work outcome, counters, usage, and whether deletion was confirmed. When
   deletion is unconfirmed the outcome is `cleanup_failed`, never the work's
   own result. Every path after step 1 does this, including create failure.

Create shares the 30-second session wall deadline. Every request has unique
`owner` and `session` labels. If its response is lost or times out, the harness
polls bounded non-deleted listings for those labels and deletes every match.
Cancellation drops an active create RPC and is treated the same way: recovery
runs, then the session returns `Canceled`. Cancellation during the baseline
drops that RPC, records any execution evidence kilnd exposes, and goes through
normal cleanup.
If an ambiguous failure produces no discoverable row before the recovery
window closes, the result is `CreateUncertain`, not a false cleanup claim.
The adapter preserves `tonic::Code` for create rejection classification; local
transport/protocol failures use `Unknown` and remain ambiguous.
Idempotent create and sandbox TTLs remain control-plane roadmap items. Forced
process death still depends on kilnd reconciliation and a future TTL.

## Model turns

The history starts as one user message. Each turn journals `model_requested`
with the SHA-256 of the request content, calls the provider under the session
deadline and cancellation, then journals the response and audits its metadata
(stop reason, call count, usage, latency). Every provider error is fatal; retry
policy belongs in network adapters.

Usage accumulates with saturating addition. Its three input counters are
disjoint: `input_tokens` is uncached input, and cache reads and cache writes
are separate; adapters normalize to this. The input budget compares
`total_input_tokens()`, the saturating sum of all three. One response over the
per-request output cap, or a cumulative total over the input or output budget,
ends the session with `TokenBudget`.

A response without tool calls is the final answer. `max_tokens` ends with
`ModelTruncated`, `refusal` with `ModelRefused`, and empty text with
`InvalidModelCall`. A response with tool calls is checked as a whole before
any call runs: `max_tokens` (arguments may be cut off) and `refusal` end the
session, as does more than the per-turn call budget. Provider call IDs must
each match the existing character rule and be unique within the turn; a bad
ID is a provider protocol fault and fails the session. Assistant text beside
tool calls shares the final-answer byte limit.

Calls then run in order. Each gets a host audit ID `call-<turn>-<index>` and
counts against the session call budget. A tool name that is not registered,
including one that breaks the name rule, becomes the model-visible result
`{"ok":false,"error":"unknown_tool"}`: it is recorded as `unknown`, runs
nothing in kilnd, and the loop continues so the model can recover. Oversized
arguments are replaced in the history by an `omitted` marker and rejected the
same way. One `tool_results` message carries every result of the turn under the
provider's call IDs.

## Tool mapping

| Tool | kilnd executions | Validation |
|---|---:|---|
| `exec` | 1 | 1–128 literal UTF-8 argv entries, no NUL, 32 KiB argv, 64 KiB stdin, at most 5 s |
| `read_file` | 2 | relative `/workspace` path; fixed `stat` then bounded `head`; at most 64 KiB returned |
| `write_file` | 1 | relative `/workspace` path; fixed `dd` argv and bounded stdin; at most 64 KiB |
| `list_dir` | 1 | relative `/workspace` directory; fixed `ls -1Ap` argv, names only |
| `workspace_diff` | 1 | fixed NUL-delimited `find`; compares at most 512 paths / 64 KiB with baseline |

No model string is interpolated into a host command. Fixed helper programs and
validated model values are separate sandbox argv entries. A model can request
`sh -c` only as explicit sandbox argv through `exec`; network remains disabled.

Defaults are eight model steps, six tool calls per session and eight per
turn, 4,096 output tokens per request, 2,000,000 input and 200,000 output
tokens per session, a 30-second session wall deadline, a 5-second tool wall deadline, a 10-second lifecycle cleanup window,
64 KiB model arguments, and 128 KiB encoded tool results. A tool reserves 250
ms of its wall budget for kilnd settlement; exceeding the wall deadline drops
the RPC to request cancellation. kilnd independently bounds retained output
and owns execution cleanup. Configured lifecycle windows are rejected above 60
seconds so deadline construction cannot overflow after a sandbox exists.

Every configuration, including limits loaded from a journal, must also stay
under hard ceilings: 1,000 steps, 10,000 tool calls and 64 per turn, a 6-hour
session, a 10-minute tool timeout, 16 MiB for each byte limit, 1,000,000
output tokens per request, and 1,000,000,000 tokens for each cumulative
budget.

## Audit

Each JSONL record has a session-local sequence, timestamp, session ID, sandbox
ID once known, and one event. Accepted model arguments and final answers, tool results, and
sandbox output are represented by SHA-256 plus byte counts or by output
evidence; their contents are excluded. Tool events use `audit_call_id`, a
host-generated ID unique within the session, to link model turns and tool
completion. Raw provider call IDs stay out of the audit; they appear only in
the model history, for result routing, and in the journal. `model_responded`
records each response's stop reason, call count, token usage, and latency.
Events retain registered tool names, kilnd execution IDs, and the sandbox ID.
Unknown model tool names are recorded as `unknown`, not copied into the log.
Oversized or invalid model actions are recorded as rejected before they are
refused or the session terminates. On a client
deadline/cancel, the harness queries recent sandbox executions to recover any
ID kilnd already assigned and copies that row's byte/truncation evidence; an
empty ID list with unknown totals means no row was observable through v1 within
the bounded recovery query. Baseline execution failures use the same recovery.

## Journal

The journal is the session's source of truth for replay. The audit stays
metadata-only and is unchanged in spirit; the journal deliberately carries
content: system prompt, task, tool definitions, limits, every model response,
and every tool call (as the model saw it after sanitization) with its result
and kilnd execution IDs.

Each JSONL record has a contiguous `sequence` from 0, a timestamp, the single
session ID, one event, and `prev_sha256`: the SHA-256 of the previous record's
exact line bytes (64 zeros for the first). Events are `session_started`,
`sandbox_ready`, `model_requested`, `model_responded`, `model_failed`,
`tool_executed`, and `session_ended`. `session_started` records every
model-visible limit (steps, calls, timeouts, token budgets, argument and
final-answer bounds, and all tool limits); lifecycle windows are not recorded.

`JsonlJournal` canonicalizes the parent and creates a new 0600 file inside the
resolved directory, refusing an existing path. That directory must be owned by
the current user and writable by no one else, and every ancestor must be owned
by root or that user and be sticky if group- or world-writable. The handle
must match the path's device and inode afterwards. It syncs data after every
record, serializes each record through a writer that stops at the line limit,
and refuses further writes after `session_ended` or a failed write. A journal write failure
ends work exactly like an audit failure; cleanup still runs.

`read_journal` reads at most 64 MiB through a bounded reader and accepts lines
up to 8 MiB. It requires schema 1, `session_started` first and only once, one
session ID, contiguous sequences, an intact chain, a newline after every
record, and nothing after `session_ended`. Writers enforce the same limits.
`complete` is true when the last record is `session_ended`.

Writers and readers also run one `Grammar` over the events. `sandbox_ready`
appears at most once, before any request, and a request needs it. Requests
run turn 1, 2, … and each is answered by exactly one `model_responded` or
`model_failed` before the next; nothing but `session_ended` follows a failure
or a response without tool calls. After a response with n calls, up to n
`tool_executed` records follow with indexes 1, 2, … and the response's call
IDs in order; fewer only when the session then ends. `session_ended` must
agree with the records: its tool-call count, last turn, and usage sum. Work
outcome `completed` needs a final response in the last turn whose text is the
final answer, the outcome `completed` needs confirmed deletion,
`cleanup_failed` means deletion was not confirmed, and a request may stay
unanswered only if the work did not complete.

`request_sha256` hashes the versioned layout `{"v", "system", "tools",
"messages", "max_output_tokens"}` encoded by serde_json. The session ID is
excluded because a replay runs under a fresh session; the turn is compared
separately.
Its `Map` is a sorted `BTreeMap` because `preserve_order` is never enabled, and
`float_roundtrip` is enabled so every float parses back to the same bits.
Equal requests therefore hash equally before and after a journal round trip.

## Replay

`ReplayProvider` loads each turn's recorded request hash with its response or
failure. For each request it checks the turn number and recomputes the hash;
on a mismatch it records `diverged_at` and returns a non-retryable
`ReplayDiverged` error, otherwise it returns the recorded response or rebuilt
error. Running out of recorded turns is a `Script` error. It rechecks the
grammar when loading and reports `exhausted()` once every turn was replayed.
The CLI prints `reproduced` only for a completed recording replayed to the
same final answer with no divergence and every turn consumed. The recorded
task and limits, which must pass the same ceilings, drive the replayed
session, which uses a new sandbox, session ID,
audit, and journal. Since the model side is fixed, a divergence means kilnd
returned different tool results than during the recording.
