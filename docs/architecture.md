# Architecture

```text
scripted, replay, or network provider (openai-compat over HTTP)
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
   own result. Every path after step 1 does this, including create failure,
   cancellation, and a create deadline whose sandbox was never observed.

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
ends the session with `TokenBudget`. Token counts come from the endpoint and
are trusted for cost, not for size: the engine also counts each response's
model output itself and ends the session with `OutputBytesBudget` once the
session total passes `max_total_model_output_bytes` (8 MiB by default,
256 MiB at most).

There are two byte measures. `ModelResponse::token_floor_bytes` is the
neutral view: the text plus, for every tool call, its call ID, its tool
name, and the compact JSON of its arguments. Adapters' usage floors count
with it. `ModelResponse::output_bytes` adds the opaque provider content
below (its model name, the compact JSON of its blocks, and its stop
details); the engine's budget and the journal grammar count with it, so
everything the journal keeps from a response is bounded. Signatures stay
out of the floor because they are large and unrelated to completion tokens,
but not out of the budget.

### Opaque provider content

Some APIs need an assistant turn sent back exactly as they returned it.
Anthropic's preserved thinking validates each `thinking` block's signature
and the position of every `fallback` block, and refuses a turn whose blocks
were edited, reordered, or dropped. So a `ModelResponse` may carry a
`ProviderContent`: the adapter's name, the model that served the turn, the
response's whole content array as received, and any stop details. The
engine copies it, unmodified, into the `assistant` message it appends, next
to the neutral `text` and `tool_calls`, which stay the sanitized view the
loop, the tools, and the grammar use. The journal grammar rebuilds history
the same way, so request hashes cover the opaque content and still verify.
The adapter that produced it echoes the blocks instead of rebuilding the
turn; other adapters ignore it. The field is skipped when absent, so
journals and request hashes from providers without it are byte-identical
to before. The audit's `model_responded` carries the served model as
`served_model` when there is one.

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

## Provider adapters

Network providers live under `src/providers/`, one module per wire API. Each
translates `ModelRequest` and `ModelResponse`, bounds what it reads, owns its
retry policy, and normalizes usage; the engine sees only the trait. They
share `src/providers/http.rs`: the locked-down client, bounded bodies and
error excerpts, the seeded retry loop, the credential scan, and the
https-unless-loopback rule described below. `openai_compat` speaks the
`/v1/chat/completions` API of the MLX server, Ollama, llama.cpp, and vLLM;
`anthropic` speaks Anthropic's Messages API. `run --provider` builds either
from its flags.

Requests are non-streaming: `{"model", "messages", "tools", "tool_choice":
"auto", "max_tokens", "temperature", "seed"?, "stream": false}`, posted to
`<base_url>/chat/completions`. The system prompt is the first message unless
it is empty. An assistant turn sends its text (or `null`) and its tool calls,
with arguments as compact JSON strings; each tool result becomes one `tool`
message with the provider's call ID and the result as compact JSON, in order.
A request without tools omits `tool_choice` as well. The serialized body is
checked against `max_request_bytes` (8 MiB by default) before anything is
sent, and a larger one is a non-retryable `ContextOverflow`.

The first choice of the response maps back. Empty content is no text. A tool
call keeps the server's `id` if it passes the engine's call-ID rule and is
otherwise named `call-<turn>-<index>`; the engine still refuses duplicates.
Arguments that parse to a JSON object are used as is; any other string is
kept raw, so the typed tool rejects it as `invalid_arguments` and the model
sees the error and can retry. `finish_reason` `stop` is `end_turn`, or
`tool_use` when tool calls are present (Ollama reports them that way);
`tool_calls` and `function_call` are `tool_use`, `length` is `max_tokens`,
`content_filter` is `refusal`, and anything else is `other`. Reasoning
side-channels (`reasoning_content`, `reasoning`) are not part of the
transcript and are dropped; their tokens still count through usage.

Usage follows the disjoint contract: `input_tokens` is `prompt_tokens` minus
`prompt_tokens_details.cached_tokens` (saturating), the cached count is
`cache_read_input_tokens`, and `output_tokens` is `completion_tokens`. A
response without `usage`, or without either count, is `InvalidResponse`: a
budget that cannot be measured is not enforced, so the session fails closed.
Usage that cannot be right is refused the same way: zero prompt tokens (the
request always carries a task), zero completion tokens for non-empty
output, or fewer completion tokens than one per 64 bytes of output, counted
as the engine counts it. Real tokenizers average a few bytes per token and
rarely exceed 16, so 64 catches only gross under-reporting.

With an API key, the adapter quotes nothing the endpoint sends. Error
messages are the status and `body withheld: authenticated request`. A
success is refused as `Rejected` ("response contained the request
credential") if the key, exactly or in its JSON-escaped spelling, appears
in the raw body, in any decoded JSON string or object key, in the parsed
text, call IDs, or tool names, or in the arguments: decoded when an object,
and otherwise both as the raw string kept and, when it is JSON of any other
type, as its decoded value. Nothing from it reaches `ModelResponse`, so
`model_failed` records only that message.

A tool name over 256 bytes is `InvalidResponse`, checked before the name is
copied; a shorter unregistered name still becomes `unknown_tool`. Server
call IDs are copied only when they pass the engine's rule (at most 64
bytes) and are otherwise replaced by `call-<turn>-<index>`.

The body is read chunk by chunk and abandoned at `max_response_bytes` (4 MiB
by default, at most 64 MiB); a declared `Content-Length` over the cap is
refused before reading. The cap counts decoded HTTP payload bytes; with no
decompression enabled that is the body exactly as the server sent it.
Transfer framing (chunk headers and extensions, HTTP/2 frames) is not
counted: it is bounded only by the session deadline and cancellation, and by
hyper's own limits, such as 16 KiB of chunk extensions per message. Error
bodies are read to at most 64 KiB, and without a key messages quote the
status and at most 512 bytes of the body, UTF-8-lossy and without control
characters. The client follows no redirects and uses no proxy, so it reads
only from the configured endpoint.

Connect errors, timeouts, other transport failures, HTTP 408, 409, 429, 500,
502, 503 and 504, and other 5xx responses whose body is not JSON are retried,
up to `max_attempts` (default 3, at most 8). The delay before attempt n+1 is
uniform in `[0, min(retry_max_delay, retry_base_delay × 2^(n-1))]` (500 ms
base, 8 s cap by default), drawn from a xorshift generator seeded from the
session ID and turn so the schedule is reproducible. `Retry-After`, as
seconds or an IMF-fixdate, replaces the jittered delay and is capped at
`retry_max_delay`. When attempts run out, the last error is returned with
`retryable: true`; the engine still ends the session on it. HTTP 400 that
mentions the context length, and 413, are `ContextOverflow`; other 4xx and
3xx are `Rejected`. None of these are retried. The engine runs `complete`
under the session deadline and cancellation, and dropping the future aborts
the request, closes its connection, and ends any backoff sleep.

The provider is recorded in `session_started` as `openai-compat` with the
model name. Replay answers from the journal and never contacts the server.

### Anthropic Messages API

`anthropic` posts to `<base_url>/v1/messages` (default
`https://api.anthropic.com`) with `x-api-key` (marked sensitive),
`anthropic-version: 2026-10-21`, and, with fallbacks on,
`anthropic-beta: server-side-fallback-2026-07-01`. The body is
`{"model", "max_tokens", "system"?, "messages", "tools"?, "tool_choice":
{"type": "auto"}?, "output_config": {"effort"}, "cache_control": {"type":
"ephemeral"}, "fallbacks": "default"?}`. It never carries `temperature`,
`top_p`, `top_k`, `thinking`, or `stream`: sampling parameters are refused
on this API version, and without `thinking` current models think
adaptively. Tools are `{"name", "description", "input_schema"}`, plus
`"strict": true` with `--strict-tools`.

A user turn is a string. An assistant turn the adapter produced is its
recorded blocks, verbatim and in order. One assistant turn from another
provider, which only a foreign transcript holds, is rebuilt as a text block
and `tool_use` blocks whose `input` is the arguments object, or `{}` when
the sanitized arguments are not an object. A turn's tool results are one
user message of `tool_result` blocks, in order, each with the result's
compact JSON and `is_error` unless the result says `"ok": true`; splitting
them across messages would teach the model to stop calling tools in
parallel. Blocks are held as JSON values, so the echo is value-for-value:
strings and numbers are exact, and object keys come out in the journal's
canonical sorted order.

The response's `content` is kept whole as the opaque blocks, with its
`model` and `stop_details`. `text` blocks, joined with `"\n"`, are the
neutral text; `tool_use` blocks are the tool calls; `thinking`,
`redacted_thinking`, `fallback`, and unknown blocks exist only in the
opaque content. A `tool_use` must have an `id` that passes the engine's
call-ID rule (the echoed block keeps it, so it cannot be replaced by a
positional ID), a name of at most 256 bytes, and an object `input`;
otherwise the response is `InvalidResponse`. `end_turn` and `stop_sequence`
are `end_turn` (`tool_use` when calls are present), `tool_use` is
`tool_use`, `max_tokens` and `model_context_window_exceeded` are
`max_tokens`, `refusal` is `refusal`, and `pause_turn` and anything else are
`other`. Stop details are recorded in the provider content, never sent
back, and never shown.

Anthropic's usage counters are already disjoint. A missing or null counter
is 0; a missing `usage` object, zero total input (not zero uncached input:
a fully cached prompt has none), zero output tokens for non-empty content,
or more than 64 bytes of neutral output per output token is
`InvalidResponse`.

Every request is authenticated, so error messages are the status, the
`error.type` from a closed list of known types (`unknown` otherwise), and a
`request_id` only when it is `req_` and ASCII letters and digits, 64 bytes
at most; never the message. 408, 409, 429, 500, 502, 503, 504, 529, other
5xx without a JSON body, and transport failures are retried as above. 413
and an `invalid_request_error` about the prompt's length or the context
window are `ContextOverflow`; every other status is `Rejected`.

Caching and ordering. The API renders `tools`, then `system`, then
`messages`, and caches by prefix. The adapter serializes all three
deterministically and never reorders or edits earlier turns, so each
request's prompt extends the previous one, and top-level `cache_control`
puts the breakpoint on the last cacheable block. Cache reads and writes come
back as `cache_read_input_tokens` and `cache_creation_input_tokens` and count
against the input budget.

Fallbacks. With `fallbacks: "default"`, a safety decline is rerun server-side
on a suitable model in the same call. The response then holds `fallback`
blocks and names the serving model in `model`; both are stored and echoed
verbatim, and `served_model` records the model in the audit. A non-streaming
response omits a declined partial, so no thinking block precedes a
`fallback` block and the verbatim echo is what the API expects. Streaming,
where it does not, is a later piece.

## Tool mapping

| Tool | kilnd executions | Validation |
|---|---:|---|
| `exec` | 1 | 1–128 literal UTF-8 argv entries, no NUL, 32 KiB argv, 64 KiB stdin, at most 5 s |
| `read_file` | 2 | `/workspace` file path; fixed `stat` then bounded `head`; at most 64 KiB returned |
| `write_file` | 1 | `/workspace` file path; fixed `dd` argv and bounded stdin; at most 64 KiB |
| `list_dir` | 1 | `/workspace` directory, `.` or `/workspace` for the root; fixed `ls -1Ap` argv, names only |
| `workspace_diff` | 1 | fixed NUL-delimited `find`; compares at most 512 paths / 64 KiB with baseline |

File paths may be relative (`notes.txt`) or start with `/workspace/`
(`/workspace/notes.txt`), the two forms models produce. The prefix is
stripped exactly once, and the rest must have no empty, `.` or `..`
component and no leading `/`. `/workspace` alone names the workspace, which
only `list_dir` accepts. Any other absolute path, including `/workspacefoo`,
`//workspace/x`, and `/workspace/../x`, is `invalid_path`. Executions start
in `/`, so the default system prompt tells the model to give `exec`
absolute paths; when kilnd's exec working directory ships, the harness will
run executions in `/workspace` instead.

No model string is interpolated into a host command. Fixed helper programs and
validated model values are separate sandbox argv entries. A model can request
`sh -c` only as explicit sandbox argv through `exec`; network remains disabled.

`HarnessConfig` defaults (used by `demo`) are eight model steps, six tool
calls per session and eight per turn, 4,096 output tokens per request,
2,000,000 input and 200,000 output tokens per session, a 30-second session
wall deadline, a 5-second tool wall deadline, a 10-second lifecycle cleanup
window, 64 KiB model arguments, and 128 KiB encoded tool results. `run`
raises the defaults for real tasks: 40 steps, 120 tool calls, a 900-second
session, a 30-second tool timeout (well under kilnd's one-hour execution
ceiling), and 16,384 output tokens per request and 600,000 per session,
because reasoning models' hidden thinking counts as output. A tool reserves 250 ms of its wall budget for
kilnd settlement; exceeding the wall deadline drops the RPC to request
cancellation. kilnd independently bounds retained output and owns execution
cleanup. Configured lifecycle windows are rejected above 60 seconds so
deadline construction cannot overflow after a sandbox exists.

Every configuration, including limits loaded from a journal, must also stay
under hard ceilings: 1,000 steps, 10,000 tool calls and 64 per turn, a 6-hour
session, a 10-minute tool timeout, 16 MiB for each byte limit, 1,000,000
output tokens per request, 1,000,000,000 tokens for each cumulative
budget, and 256 MiB of model output per session.

## Audit

Each JSONL record has a session-local sequence, timestamp, session ID, sandbox
ID once known, and one event. Accepted model arguments and final answers, tool
results, and sandbox output are represented by SHA-256 plus byte counts or by
output evidence; their contents are excluded. Tool events use `audit_call_id`,
a host-generated ID unique within the session, to link model turns and tool
completion. Raw provider call IDs stay out of the audit; they appear only in
the model history, for result routing, and in the journal. `model_responded`
records each response's stop reason, call count, token usage, and latency.
Events retain registered tool names, kilnd execution IDs, and the sandbox ID.
Unknown model tool names are recorded as `unknown`, not copied into the log.
Oversized or invalid model actions are recorded as rejected before they are
refused or the session terminates. On a client deadline/cancel, the harness
queries recent sandbox executions to recover any ID kilnd already assigned and
copies that row's byte/truncation evidence; an empty ID list with unknown
totals means no row was observable through v1 within the bounded recovery
query. Baseline execution failures use the same recovery.

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

The journal and the audit share one opener. It refuses a path whose raw bytes
are empty, end in `/`, or end in `.` or `..`, resolves the parent once to an
absolute starting path, then walks it from `/` through pinned descriptors: each
component is opened relative to the previous directory with `O_RDONLY |
O_DIRECTORY | O_NOFOLLOW`, created with mode 0700 if missing (set again with
`fchmod`), and checked with `fstat`. Every directory must be readable and
searchable by the effective user (the walk opens it), owned by root or that
user, and sticky if group- or world-writable, and the final one must be owned
by that user and writable by no one else. The file is created relative to that
verified descriptor with `O_CREAT | O_EXCL | O_NOFOLLOW` (an existing audit is
reopened for appending) and mode 0600, also set with `fchmod`, so swapping a
directory by path cannot redirect it. On Linux, ACL entries for named users and
groups appear in the group mode bits as the ACL mask, so the mode check covers
them, and the explicit modes reset the mask of what is created, capping an
inherited default ACL. On macOS the rule checks ownership and mode bits only;
extended ACLs are not inspected. A refusal names the directory, its mode, and
the fix. The opener records each directory it creates with its pinned parent
descriptor, name, and identity; if anything later fails, it removes the new
file and then the new directories, newest first, through those descriptors and
only if `fstatat` still shows the same empty entry. What rollback keeps is
taken before each `mkdirat`, and an entry created but not yet recorded is
removed by name at once, so even running out of descriptors part-way through a
deep path leaves nothing behind. `discard` does the same for
an output nobody has written to yet. The journal syncs data after every record,
serializes each record through a writer that stops at the line limit, and
refuses further writes after `session_ended` or a failed write. A journal write
failure ends work exactly like an audit failure; cleanup still runs.

`read_journal` opens the source with `O_NOFOLLOW | O_NONBLOCK`, requires a
regular file by `fstat` on that descriptor, and only then makes it blocking, so
a FIFO cannot stall it. It reads at most 64 MiB through a bounded reader and
accepts lines up to 8 MiB. It requires schema 1, `session_started` first and
only once, one session ID, contiguous sequences, an intact chain, a newline
after every record, and nothing after `session_ended`. Writers enforce the same
limits. `complete` is true when the last record is `session_ended`.

Writers and readers also run one `Grammar` over the events. The recorded task
must pass the engine's task checks and the recorded limits its hard ceilings.
`sandbox_ready` appears at most once, before any request, and a request needs
it. Requests run turn 1, 2, … and each is answered by exactly one
`model_responded` or `model_failed` before the next; nothing but
`session_ended` follows a failure or a response without tool calls. After a
response with n calls, up to n `tool_executed` records follow with indexes 1,
2, …, and each call must equal the response's call after the shared
`sanitize_call` (unregistered names become `unknown`, oversized arguments the
`omitted` marker); fewer only when the session then ends. The grammar rebuilds
the message history from the task, each response, and each tool result exactly
as the engine does, and every `model_requested` must carry that history's
message count and request hash, so every result that fed a later request is
bound to its record. Results of a final tool turn feed no later request; only
replay checks those. `session_ended` must agree with the records: its tool-call
count, last turn, and usage sum. Work outcome `completed` needs a final
response in the last turn whose text is the final answer, the outcome
`completed` needs confirmed deletion, `cleanup_failed` is required exactly when
deletion was not confirmed, and a request may stay unanswered only if the work
did not complete.

The grammar also applies the engine's acceptance rules with the recorded
limits. Requests stop at `max_steps`, and tool records at `max_tool_calls` in
total and `max_tool_calls_per_turn` in a turn. A response the engine refuses (a
token budget, the per-request output cap, or the model-output byte budget
exceeded, a `max_tokens` or
`refusal` stop, invalid or repeated call IDs, too many calls, or text over its
byte bound) may be recorded, because the engine records it before refusing it,
but nothing but `session_ended` may follow it and it cannot complete the
session.

`request_sha256` hashes the versioned layout `{"v", "system", "tools",
"messages", "max_output_tokens"}` encoded by serde_json. The session ID is
excluded because a replay runs under a fresh session; the turn is compared
separately.
Its `Map` is a sorted `BTreeMap` because `preserve_order` is never enabled, and
`float_roundtrip` is enabled so every float parses back to the same bits.
Equal requests therefore hash equally before and after a journal round trip.

## Replay

`ReplayProvider` loads each turn's recorded request hash with its response or
failure. For each request it checks the turn number and recomputes the hash; on
a mismatch it records `diverged_at` and returns a non-retryable
`ReplayDiverged` error, otherwise it returns the recorded response or rebuilt
error. A recording that ends with an unanswered request keeps it as a turn to
reach: replaying it returns a `Script` error and accounts for it. A request
past the end of the recording is also a `Script` error, and it sets
`diverged_at` like any other request or call the recording does not match.
After each tool call is journaled, an interrupted one included, the engine
calls `Provider::tool_completed`; replay compares the sanitized call, `ok`, and
result with the next recorded `tool_executed` and diverges at that turn on a
mismatch, before the result reaches another request. A call or request the
recording does not match counts as unexpected. It rechecks the grammar when
loading, and `exhausted()` is true only when every turn and tool call was
matched and nothing unexpected happened. The CLI prints `reproduced` only for a
completed recording replayed to the same final answer with no divergence and
everything consumed. Before contacting kilnd it opens the source read-only and
both outputs, and compares device and inode by `fstat` on the open descriptors;
path checks before that are diagnostics only. On a clash it discards both
outputs, which removes only what their opens created, through the pinned parent
descriptors. The recorded task and limits, which must pass the same ceilings,
drive the replayed session, which uses a new sandbox, session ID, audit, and
journal. Since the model side is fixed, a divergence means kilnd returned
different tool results than during the recording.
