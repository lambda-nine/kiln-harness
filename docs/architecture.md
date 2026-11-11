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
   prompt at most 64 KiB each, nonempty prompt, and any seed and check; see
   [Eval tasks](#eval-tasks)) and calculate one absolute session deadline.
   A task with a check also gets a work deadline `check.timeout` earlier;
   everything up to the final answer uses it.
2. Discover the daemon: one `Health` call under that deadline and
   cancellation. A daemon that is not `SERVING` is refused, and so are
   incoherent advertised limits and an expiry the daemon's bounds would cut
   below the session or outside what the journal can record (below). Nothing
   is recorded on these paths, since nothing exists yet. Then open the audit
   session, with the listed features, and journal `session_started`.
3. Create exactly one sandbox with network none, 256 MiB memory, 0.5 CPU,
   64 PIDs, 256 MiB disk, read-only root, writable `/workspace`, and, when
   the daemon lists sandbox expiry, an expiry policy.
4. Write the task's seed files, if any, then capture a bounded `/workspace`
   file-set baseline through kilnd and journal `sandbox_ready`.
5. Alternate model turns and their tool results until a final answer or a
   budget/cancellation/error ends work. Each provider call runs beside a
   renew heartbeat when the daemon supports expiry.
6. Retry idempotent `DeleteSandbox` and independently verify absence with a
   complete non-deleted listing after every acknowledgement or ambiguous error.
   Cleanup has a separate 10-second wall window because kilnd may need to drain
   descendants and publish terminal state after the work deadline. Failure to
   confirm deletion is returned as a cleanup error.
7. Write audit `session_finished` and journal `session_ended` with the outcome,
   the work outcome, counters, usage, and whether deletion was confirmed. When
   deletion is unconfirmed the outcome is `cleanup_failed`, never the work's
   own result. Every path after step 1 does this, including create failure,
   cancellation, a create deadline whose sandbox was never observed, and a
   sandbox that expired.

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

### Create retries

When a serving daemon lists `FEATURE_IDEMPOTENT_CREATE`, the create is keyed:
`request_id` is `kiln-harness:<session ID>`, checked against kilnd's grammar
(1 to 64 bytes of `0x21`-`0x7e`) before anything is recorded. kilnd then
answers the original call and every retry with the same key and an equal
request with the same sandbox, in whatever state it settled. The request is
built once and cloned for each attempt, never rebuilt, because kilnd hashes the
request as sent and answers any difference `ALREADY_EXISTS
request_id_conflict`. A daemon that does not list the feature gets today's
create: no `request_id`, one attempt, nothing released.

- **What is retried.** Only a keyed create, and only after `UNAVAILABLE` (any
  reason, `create_unsettled` included), `UNKNOWN` (local transport and protocol
  failures), `DEADLINE_EXCEEDED` (from the server, `create_in_flight`
  included), `ABORTED`, `INTERNAL` or `CANCELLED`. Every other code is final at
  once, notably `INVALID_ARGUMENT`, `RESOURCE_EXHAUSTED request_id_quota`,
  `ALREADY_EXISTS request_id_conflict`, `FAILED_PRECONDITION`,
  `PERMISSION_DENIED`, `UNAUTHENTICATED`, `UNIMPLEMENTED` and `NOT_FOUND`, and
  goes through recovery and classification as before.
- **No per-attempt timeout.** An attempt has no client timeout and sends no
  `grpc-timeout`: it runs until it answers, bounded only by the session
  deadline and cancellation, as an unkeyed create always has. So the harness
  never abandons an attempt it might then retry while the first is still
  running; the only way out of an attempt is ending the whole create, which
  goes to label recovery and never releases the key.
- **Health before every retry.** After the backoff the harness reads Health
  again and retries only if the daemon is serving and still lists idempotent
  create. A daemon restarted into an older build would ignore `request_id` and
  could make a second sandbox; one that is not serving, or no longer lists the
  feature, ends the retries with the last create error. A Health error backs
  off and reads again without using an attempt. The read narrows the gap but
  cannot close it: the Create that follows is a separate RPC and may reach a
  daemon that replaced the one that answered. An older daemon's answer echoes
  no request ID and fails the settled check below, and label recovery deletes
  both sandboxes (see the threat model's deliberate limits).
- **Bounds.** At most 8 create RPCs. The backoff before the nth sleep after the
  first failure (Health rereads included) is `lifecycle_retry_delay × 2^(n-1)`,
  at most 2 s. The retry phase ends `lifecycle_timeout` after the first failure:
  a backoff that would end at or after that point is not taken, and the last
  create error is the result, as it is when the attempts run out. Every sleep
  and Health read waits under the same cancellation and session deadline as the
  create, so either one during a backoff or Health read is `Canceled` or
  `Deadline`, exactly as during the create. Cancellation wins every tie: each
  wait polls it first, and it is checked again after every Create and Health
  answer, before the attempt cap, the window, the deadline or the retry rules
  can end the phase, and before the next attempt is counted. A create that
  answered with a sandbox is still returned, and the session's cleanup deletes
  it by ID.
- **Settled state.** An `OK` answer is accepted only `READY` and, when keyed,
  echoing this session's `request_id`. Any other state (`FAILED`, `DELETED`,
  `CREATING`, `DELETING`, `LOST`, `QUARANTINED`, unknown) or a mismatched or
  empty echo is a create failure, never retried: a local error naming the
  sandbox, its state and kilnd's `failure_reason` (`FAILED_PRECONDITION`, or
  `UNKNOWN` for an echo mismatch, with no `kilnd-reason`), work outcome
  `create_failed`. Label recovery runs as for any failed create, and absence is
  what its listing proves, not the state kilnd reported; if recovery cannot
  confirm it the outcome is `cleanup_failed`. The same check guards an unkeyed
  create, which kilnd never answers `OK` without `READY`.
- **Release.** With `FEATURE_REQUEST_ID_RELEASE` also listed (decided once, at
  session start), the normal cleanup after a `READY` create that took exactly
  one attempt asks kilnd to give up the key: every delete sends
  `release_request_id`, and once absence is confirmed, if no delete reported
  the key released (one that left the row `DELETING` releases nothing and
  kilnd remembers nothing), up to two more deletes are sent within the cleanup
  window, `lifecycle_retry_delay` apart.
  kilnd's contract allows release only while the client holds the create
  response for that key with no retry pending or to come, and only after
  absence is confirmed; the normal cleanup is the one place that holds, so label
  recovery and the create-failure path never release. After a retried create
  an earlier attempt the harness gave up on may still reach kilnd, so that
  cleanup keeps the key: every delete sends `release_request_id = false`, no
  extra deletes are sent, and `request_id_released` is absent, as when release
  was never asked for. A late attempt then finds kilnd's retained `DELETED`
  row and creates nothing. Release never makes cleanup fail: absence is the
  cleanup result, and `session_finished` reports `request_id_released` beside
  it.

`sandbox_created` records `request_id` (only when keyed) and `create_attempts`;
`sandbox_recovered` records `create_attempts`. The journal is unchanged, and a
replay is a new session with its own key.

## Eval tasks

A session task may carry a seed and a check besides its system prompt and
prompt. The eval suite (see `evals/README.md`) loads both from a task
directory; any caller of `Harness::run` may set them.

- **Seed.** At most 256 files and 4 MiB in total, each at most the file
  limit, with paths in the normalized relative form the file tools accept
  (no `/workspace/` prefix, no empty, `.` or `..` component, no control
  characters), strictly ascending, and no file inside another file's path.
  In `ToolRegistry::start_session`, before the baseline, each file is written
  in order with exactly the `write_file` Exec (`/bin/dd of=<path>
  status=none`, the bytes on stdin), one Exec per file, after a fixed
  `/bin/mkdir -p -- /workspace/<dir>` for each parent directory not made yet.
  Each Exec is bounded by the tool timeout and the work deadline, and the
  whole step by cancellation like the baseline. Any failure (an RPC error, a
  non-zero exit, an unusable sandbox, the deadline) ends the session with
  `HarnessError::Seed` and work outcome `failed` before any model call, and
  cleanup deletes and verifies as for any failure. The baseline therefore
  includes the seed, so `workspace_diff` shows only the agent's changes. The
  audit records `workspace_seeded` (files, bytes, execution IDs) once seeding
  succeeds, only for a task with a seed.
- **Check.** A script of 1 byte to 64 KiB (and at most the tools' stdin
  limit) with a timeout of 1 to 600 seconds, whole milliseconds, at most half
  the session timeout. The model's work deadline is the session deadline less
  the check timeout: the loop, its provider calls, its tools, seeding, create
  and discovery all use it, so the check always has its window before the
  session deadline. The sandbox's expiry lifetime is computed from the whole
  session timeout as before. The check runs once, only after the loop returns
  a final answer that passed the final usability check, before cleanup: one
  Exec of `["/bin/sh", "-s"]` with the script on stdin, under the check
  timeout less the usual 250 ms settlement reserve, capped by the session
  deadline, inside a biased `select!` on cancellation and the session
  deadline. When the daemon supports expiry, a due renew runs first, best
  effort. Cancellation wins over any verdict, as in the final usability
  check: a token already canceled before the renew or the check sends no
  check, and one canceled by the time the check answers turns its verdict
  into `canceled`, keeping the execution ID and output. Exited 0
  is `passed`; another exit or a timed-out execution is `failed`; any other
  execution state, an RPC error, cancellation (`canceled`) or the deadline
  (`deadline`) is `error`. There is no lost-response recovery for the check:
  an error is reported as one. The check never changes the work outcome: a
  completed session whose check fails is still `completed`, and
  `SessionOutcome::check` carries the result. The journal's `check_completed`
  keeps each output stream decoded as lossy UTF-8 and then cut at a
  character boundary to at most 4 KiB, so replacement characters cannot
  grow it past the bound; the audit's
  `check_completed` has only the result, exit code, whether it timed out, the
  script's hash and size, the execution ID and output evidence. The script
  never enters a model request.

## Discovery and typed reasons

`Health.runtime_capabilities` maps to `Capabilities`: a set of `Feature`s
(the ones the harness knows by name, and `Other(n)` for the rest) and the
expiry bounds from `ApiLimits` (minimum, maximum and default lifetime and
idle). An older daemon lists no features and no limits, which reads as
unsupported everywhere. The engine reads them once per session, before
create, and only from a `SERVING` response, because an older daemon ignores
request fields it does not know: an expiry sent to one would silently never
apply. `session_opened` records the feature names (the lower-case suffix
after `FEATURE_`), metadata only. The CLI's `connect` only opens the
transport, so this is the one Health call before a session, bounded like the
rest of it. After the session the CLI checks Health once more, within the
lifecycle window and at most 10 seconds, and only warns on stderr if kilnd
does not answer or is not serving; it never changes the exit status.

The advertised limits must be coherent before anything is created: for
lifetime and idle each, a nonzero maximum is at least the minimum, and a
nonzero default lies within `[min, max]`. Otherwise the session fails with
an `Api` error (`FAILED_PRECONDITION`, "kilnd advertised inconsistent expiry
limits"), since no value can be clamped into an empty range.

With `TYPED_REASONS`, kilnd's refusals carry a `kilnd-reason` metadata
entry. The adapter copies it into `ApiError::reason` only when it is 1 to
128 ASCII bytes of `[a-z0-9_:]`; anything else is no reason. The status
message starts with the same token, but the harness never parses prose. The
engine reads a reason only when the session's daemon lists the feature, and
otherwise behaves exactly as without one. Two classifications use it:

- `FAILED_PRECONDITION` with `expired_idle`, `expired_lifetime` or
  `sandbox_deleted` on any sandbox RPC (the baseline, a tool's executions,
  or a renew) means the sandbox is gone. The session ends with
  `HarnessError::SandboxExpired`, work outcome `failed`, and the normal
  cleanup barrier still runs. Delete of an expired row succeeds, and the row
  has left the default listing, so absence verifies. A tool call that hits
  it is journaled with the same model-visible `kilnd_error` result as
  without typed reasons, and the model is not asked again.
- `UNIMPLEMENTED` with a `backend_unsupported:<feature>` reason, the
  feature named, is a definite create rejection: kilnd refuses it before
  any row or backend call. `UNIMPLEMENTED` without one, with an empty
  feature, or with any other reason, stays ambiguous.

A successful Exec RPC does not mean the sandbox survived. kilnd says so in
`Execution.sandbox_reusable`, and the adapter keeps it with the execution's
`cancel_reason`. After any execution the harness sends, a tool's, the
baseline's, or a workspace manifest's, `sandbox_reusable == false` ends the
session once that tool's result is journaled, and no further execution or
model call happens. The same holds for a row recovered after a lost Exec
response, but only terminal evidence counts: a RUNNING row can still be
canceled or retire the sandbox, so recovery follows every new row with
`GetExecution`, and reads `GetSandbox` again for further rows, until each
is terminal or the recovery deadline passes. Recovery is best-effort: any
finite window can miss a row published just after its last read, so it
only stops a session early, and the final check below is what gates a
completed session. It returns early only once it has found at least one
new row and every one is terminal; otherwise it runs to its last pass,
which starts `min(100 ms, window / 4)` before its deadline so its reads
have that long to answer. Polls back off from 10 ms to 200 ms;
`GetSandbox` is read on the first pass, while no row is known, every
fourth pass after that, and on the last pass, so one unsettled row costs
about ten reads a second, not two hundred. If no `GetSandbox` read
succeeded in the whole window, or the last one failed with nothing found,
nothing is known about what kilnd assigned, and the session ends the same
way, "execution evidence could not be read after its response was lost";
a read that succeeded and found nothing keeps the call an ordinary
failure. A cancellation that arrives during recovery ends it at once, and
the session ends as canceled; recovery after a cancellation still runs its
window, to record what kilnd assigned. Settled rows keep their
`sandbox_reusable` and `cancel_reason`, and one that left the sandbox
unusable stops the session exactly as a returned one would, during the
baseline too. A row still not terminal at the deadline fails closed: the
tool result is journaled, then the session ends with an `Api`
`FAILED_PRECONDITION` error, "execution <id> did not settle after its
response was lost", and no later Exec runs. A cancel reason of `SANDBOX_DELETED` or
`SANDBOX_EXPIRED_LIFETIME` ends it as `SandboxExpired` (`sandbox_deleted`
or `expired_lifetime`); any other cause, or none, fails closed with an
`Api` error, "execution left the sandbox unusable". Either way cleanup still
deletes and verifies absence. An older daemon does not send the field, and
then nothing changes.

The final check gates `completed`. kilnd refuses every Exec once the
sandbox is not READY, and the harness stops on that refusal; what remains
is the end of the session, when no further Exec would reveal it. So after
a final answer, and before `session_finished` and `session_ended`, one
`GetSandbox` (`SandboxApi::final_check`) must confirm the sandbox stayed
usable, in every session. It is bounded by the session deadline and the
lifecycle window, and canceled with the session. The sandbox must be
READY, and, from a daemon that reports execution usability (one that lists
sandbox expiry), every recent execution the session never saw must be
terminal and reusable. A row that left the sandbox unusable ends the
session with its mapped stop, as a returned one would; a row still
running, or a sandbox not READY, ends it as `Api` `FAILED_PRECONDITION`,
"sandbox was not usable when the session finished", or as `SandboxExpired`
when kilnd's deletion reason names an expiry or deletion; and a check that
cannot be made ends it as `Api` `UNAVAILABLE`, "could not confirm the
sandbox at the end of the session". Cancellation and the session deadline
win over its verdict before the check starts, while it runs, and after it
answers. An older daemon's sandbox is checked by state only. The final
answer stays journaled as the model's response and the terminal records
say `failed`; the journal gains no record, so its grammar is unchanged.
The audit gets a metadata-only `final_check` event: the state (`ready`,
`not_ready`, or `unknown`), how many unseen executions there were, and
whether the sandbox was usable. Replay runs the same check against its
fresh sandbox. Every `GetSandbox` answer must name the sandbox asked
about; anything else is a protocol error (`INTERNAL`), never an empty
read.

An Exec refused with `FAILED_PRECONDITION` and a typed `sandbox_failed`,
`sandbox_lost`, `sandbox_quarantined` or `sandbox_not_ready` reason proves
the same thing: this session's sandbox, which was ready, cannot take
another call. The model-visible result is still the `kilnd_error` it was,
journaled first; then the session ends with the same `Api` error. A renew
refused with one of these reasons is terminal too, never retried. One
exception: kilnd also sends `sandbox_not_ready`, saying to retry, when an
Exec lost a race with its own row insert on a READY sandbox. So on an Exec
refused `sandbox_not_ready` the harness reads the sandbox once, within the
call's deadline; only a sandbox that reads READY turns the refusal into an
ordinary, recoverable `kilnd_error`, and anything else, a failed lookup
included, stays terminal. `sandbox_failed`, `sandbox_lost` and
`sandbox_quarantined` are terminal unconditionally, and so is any of the
four on a renew. The expiry and deletion tokens still map to
`SandboxExpired`, and without typed reasons nothing changes.

## Sandbox expiry and the renew heartbeat

When the daemon lists `SANDBOX_EXPIRY`, create sends
`ExpiryPolicy { max_lifetime_ms, idle_timeout_ms }`:

- `max_lifetime_ms` is `session_timeout + lifecycle_timeout +
  sandbox_lifetime_margin`: the session, its cleanup window, and a margin
  (60 s by default, 1 s to 10 min) for the short evidence recoveries that
  can follow a deadline. kilnd counts it from `READY`, after create
  started, so it is conservative.
- `idle_timeout_ms` is `sandbox_idle_timeout` (120 s by default, 10 s to
  1 h).
- Both are clamped into the daemon's `[min, max]`, where a maximum of 0 is
  no cap. A lifetime clamped below `session_timeout + lifecycle_timeout` is
  refused before create with `InvalidTask("daemon max lifetime is shorter
  than the session")`: a sandbox certain to expire mid-session is a
  configuration error. A clamped policy outside the bounds the journal
  grammar accepts (below) is refused before create as an `Api` error, so
  the engine never sends an expiry it could not record.

The audit's `sandbox_created` records the policy sent as
`expiry_lifetime_ms` and `expiry_idle_ms`, or `null` for both when the
daemon does not support expiry. That is the degraded mode: no expiry is
sent and the sandbox never expires. The journal's `sandbox_ready` carries
the same two fields, skipped when absent, so journals without expiry are
byte-identical to before. Expiry is not model-visible, so it is not in
`SessionLimits`, no request hash covers it, and replay uses its own
configuration. The grammar accepts the pair or neither, and checks bounds,
not exact equivalence, because the margin, the cleanup window, and the
daemon's clamping are not recorded: the lifetime must exceed the recorded
session timeout and be at most the sum of the session, lifecycle, and margin
ceilings (6 h + 60 s + 10 min), and the idle timeout must be from kilnd's
10-second floor to 1 hour. A forged expiry within those bounds verifies.

kilnd counts an execution's start and finish as activity and pauses idle
while one runs, so tool calls need nothing more. A provider call has no
execution and can take minutes, so `complete_with_heartbeat` runs it beside
a `tokio::select!` loop that sends `RenewSandbox` at
`interval = max(ceil(idle / 3), 1 s)`. The schedule is session-wide, not per
call: the tool session records the last activity it can prove kilnd saw
(the sandbox becoming ready, an Exec that returned an execution, a
terminal execution row found by recovery after a lost response, and every
successful renew), and a renew is due one interval after it. kilnd commits
that activity somewhere between the request and the reply, so each is
anchored at the moment its request was sent: create for readiness, the
Exec for a returned or recovered execution, and the renew itself. A late
reply cannot move the anchor past kilnd's, and the anchor never moves
back. This is pessimistic for long commands: after a 29-second command the
anchor is 29 seconds old, so the next kilnd-bound tool may force a
renew. A failed Exec RPC with no settled recovered row
proves nothing and leaves the time unchanged; while an admitted execution
runs, kilnd pauses idle itself. So several short, execution-free turns
cannot add up to the idle timeout. An overdue renew runs, through one
bounded `renew_if_due`, before each provider call and before each tool call
that will reach kilnd, so a refused Exec cannot be followed by another in
the same turn without one; tool calls the host answers itself (an unknown
tool, invalid arguments, a budget refusal) renew nothing. Nothing renews
while a tool call runs, and kilnd sees no activity from an Exec it never
admits, so two rules bound one call. Before create, the tool budget (the
tool timeout plus the response-loss recovery window, at most 1 s) plus the
renew interval must fit in the idle timeout actually sent after kilnd's
clamp, or the session is refused with `InvalidTask` naming both values; the
defaults (30 s tools, 120 s idle, 40 s interval) fit, and a 10 s idle
timeout allows tools of about 5 s. The tool timeout bounds the tool's
Execs together, not each: a tool that runs several (`read_file`) gives each
the time that remains, and a further one with none left ends the call
through the tool `Deadline` path. Response-loss recovery may follow, for
its window, so the budget used for the expiry fit is the tool timeout plus
recovery, and the whole call ends by `min(session deadline, tool start +
budget)`, never after the session deadline. So the budget is the true worst
case for every tool. And before a kilnd-bound tool call, a renew is forced
whenever `now + budget` would reach `last activity + idle`, whatever the
backoff after a failed renew says, and the tool starts only after it
succeeds: a transient failure is retried after the lifecycle retry delay,
counting toward the failure budget, until the earlier of the session
deadline and the estimated idle deadline, and then the session ends with
`renew_failed`. Each forced attempt is cut off at that same instant, so
none runs past it. The tool's `model_turn` audit record
is written before this renew, so a fatal renew leaves the call on record
with no completion; if the session deadline has passed, or passes while
the renew is in flight, the session ends with `Deadline`, not
`renew_failed` or `SandboxExpired`. During a provider
call, the loop then waits for the next due time. Each renew RPC is bounded
by the next tick and the session deadline. Nothing renews while an
execution runs. The loop is not a
spawned task: dropping the call, on an answer, a deadline, cancellation, or
an error, drops any renew in flight, so nothing outlives it. A successful
renew counts toward `renews` in `SessionOutcome` and `session_finished`;
heartbeats write no other records. A renew refused with an expiry reason
drops the provider call and ends the session with `SandboxExpired`.
`UNAVAILABLE`, `DEADLINE_EXCEEDED`, `UNKNOWN` (which local transport
failures use), `RESOURCE_EXHAUSTED` (kilnd's connection and RPC admission,
which clears on its own; this applies to renew only), and a renew that does
not answer before the next tick are transient. They are counted across
provider calls, a failed renew is retried an interval after it started, and
a success or any later activity resets the count. Activity is ordered by a
generation the tool session bumps on every record, captured when a renew
starts, so an Exec in the same clock tick as a failed renew still resets
it. Then
`ceil(idle / interval) - 1` in a row (2 for any idle of 3 s or more) end
the session with `SandboxExpired { reason: renew_failed }`, since by then
the sandbox has probably expired. A renew response must name the sandbox
renewed, still READY; anything else is a protocol error, `INTERNAL`, and
terminal. Any other error is fatal as an `Api` error. Replay runs
the same engine without provider latency, so it rarely renews, and the
journal does not change.

Forced process death no longer leaks a sandbox on a daemon with expiry:
kilnd reclaims it within the idle timeout, or at most at its lifetime.

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
`anthropic-version: 2026-10-21`, and, only with `--fallbacks`,
`anthropic-beta: server-side-fallback-2026-07-01`. The body is
`{"model", "max_tokens", "system"?, "messages", "tools"?, "tool_choice":
{"type": "auto"}?, "output_config": {"effort"}, "cache_control": {"type":
"ephemeral"}, "fallbacks": "default"?}`. It never carries `temperature`,
`top_p`, `top_k`, `thinking`, or `stream`: sampling parameters are refused
on this API version, and without `thinking` current models think
adaptively. Tools are `{"name", "description", "input_schema"}`. With
`--strict-tools` each also carries `"strict": true`, and its schema is
rewritten keyword by keyword. Supported keywords (`type`, `properties`,
`required`, `additionalProperties`, `items`, `anyOf`, `enum`, `const`,
`description`, `title`, `default`) are kept, and `minItems` of 0 or 1.
Size and range bounds strict mode refuses (`minItems` above 1, `maxItems`,
`minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `minLength`,
`maxLength`) are removed, because the typed tools enforce them; for the
built-in tools that is `exec.argv.maxItems`, `exec.timeout_ms.minimum`, and
`read_file.max_bytes.minimum`, while `exec.argv.minItems: 1` stays. An
object schema without `required` gets `"required": []`. Any other keyword
(`pattern`, `multipleOf`, `$ref`, `$defs`, `format`, `oneOf`, ...), tuple
`items`, an `enum` or `const` member that is not a string, integer,
boolean, or null, or an object schema without
`"additionalProperties": false` is refused with its path rather than
dropped, since nothing would enforce it, and the built-in tools are checked
this way when the provider is built. The subset is deliberately narrow and
fails closed: it refuses some constructs the API's strict mode may accept,
because the harness's tools need none of them. The canonical schemas in
`ToolDefinition`, the journal, and the request hash are unchanged, and the
typed tools still enforce every removed bound.

A user turn is a string. An assistant turn the adapter produced is its
recorded blocks, verbatim and in order. One assistant turn from another
provider, which only a foreign transcript holds, is rebuilt as a text block
and `tool_use` blocks whose `input` is the arguments object, or `{}` when
the sanitized arguments are not an object. A turn's tool results are one
user message of `tool_result` blocks, in order, each with the result's
compact JSON and `is_error` unless the result says `"ok": true`; splitting
them across messages would teach the model to stop calling tools in
parallel. Blocks are held as JSON values, so the echo is value-for-value:
strings are exact, object keys come out in the journal's canonical sorted
order, and numbers are exact because a response whose opaque content holds
one serde_json cannot hold exactly (a float, or an integer outside 64 bits)
is refused, `tool_use` inputs included, since they are echoed from the
blocks too. Current block types carry none, and the registered tools take
only strings, string arrays, and integers, so nothing valid is lost.

The response must be the success envelope: `type: "message"`,
`role: "assistant"`, a valid `model` (now required), a `content` array, and
a known `stop_reason`. A `type: "error"` body is an error even with a
`content` array. The response's `content` is kept whole as the opaque blocks, with its
`model` and `stop_details`. `text` blocks, joined with `"\n"`, are the
neutral text; `tool_use` blocks are the tool calls; `thinking`,
`redacted_thinking`, `fallback`, and unknown blocks exist only in the
opaque content. A `tool_use` must have an `id` that passes the engine's
call-ID rule (the echoed block keeps it, so it cannot be replaced by a
positional ID), a name of at most 256 bytes, and an object `input`;
otherwise the response is `InvalidResponse`. `end_turn` and `stop_sequence`
are `end_turn`, `tool_use` is `tool_use`, `max_tokens` and
`model_context_window_exceeded` are `max_tokens`, and `refusal` is
`refusal`. Since only client tools are sent, a `tool_use` stop must come
with at least one `tool_use` block and `end_turn` or `stop_sequence` with
none; either mismatch is `InvalidResponse`, not normalized. A truncated or
refused turn may still hold blocks, and the engine refuses it. Provider
neutrally, the engine and the grammar treat a `tool_use` stop with no tool
calls as a protocol fault (`InvalidModelCall`), never a final answer; an
openai-compat `finish_reason: "tool_calls"` with no calls ends that way. `pause_turn` is `InvalidResponse`:
it needs server tools, which the harness never sends, and treating it as a
final answer would end the turn early. A missing or unknown stop reason is
`InvalidResponse` too; the adapter never produces `other`. Stop details are recorded in the provider content, never sent
back, and never shown.

Anthropic's usage counters are already disjoint. A missing or null counter
is 0. The top-level counters are always validated: they describe the
attempt that produced the returned message. With fallbacks on,
`usage.iterations` lists every attempt, each billed; the live API sends it
on every response, with one `message` entry whose `model` is null when no
fallback ran. With fallbacks off, the live API sent top-level usage only. When the array is present, every entry needs a known `type`
(`message` or `fallback_message`), valid counters, and nonzero input. A
`fallback_message` must name a valid `model`; a `message` may leave it null
or absent, meaning the requested model. The last entry must be the serving
attempt: the response's `model` (an entry without one matches only when
the response's `model` is the requested one) and exactly the top-level
counters. When any entry is a `fallback_message`, there is exactly one and
it is last, after `message` attempts only; a lone `fallback_message` is a
sticky-routed turn. With fallbacks requested, the array is required: a
response without it, or with `null`, is `InvalidResponse`, since it would
hide every attempt but the serving one. Without fallbacks the array may be
missing, and a `fallback_message` entry or a `fallback` block is refused,
because the request asked for neither. When the content holds `fallback`
blocks, they must describe the same chain as the usage: the last entry is
a `fallback_message`, the last block's `to.model` is its model and the
response's `model`, and the first block's `from.model` is the requested
model. Every block also proves an attempt that declined, so each must
have its own ordinary `message` entry, in order, whose model (null for the
first means the requested model) is that block's `from.model`, before the
one terminal `fallback_message`: with client tools only, one non-streaming
call has no other iterations, so the array holds exactly one entry per
block plus one. And since a non-streaming response omits a declined
partial, any block before the last `fallback` block is refused as content
before the fallback boundary; a declined model's tool call can never run
under the fallback's identity. Only `from.model` and `to.model` are read,
as identifiers; the blocks are echoed verbatim. A lone `fallback_message`
with no block is a sticky-routed turn and stays valid. Other usage fields (`cache_creation`,
`output_tokens_details`, `service_tier`, and the like) are ignored. The
zero-output check exempts `fallback` blocks, which are boundary metadata
rather than generated output, so a final fallback that declined before
output is a refusal, not invalid usage; every other block, unknown ones
included, still needs a token, and `fallback` blocks still count as bytes. A response whose two views of the serving
attempt disagree is `InvalidResponse`; neither side is picked. The usage is
then the saturating sum of every entry, without adding the top level
again; without the array it is the top level. The per-request output cap therefore covers one model
call including its server-side attempts; without `--fallbacks`, the
default, it is a strict per-attempt cap. A missing `usage` object, zero
total input (not zero uncached input: a fully cached prompt has none), zero
output tokens for non-empty content, or more than 64 bytes of neutral
output per output token is `InvalidResponse`. The output checks use the
serving attempt's count, since it produced the returned content; the
budgets use the sum.

With a key, error messages are the status, the `error.type` from a closed
list of known types (`unknown` otherwise), and a `request_id` only when it
is `req_` and ASCII letters and digits, 64 bytes at most, and each only
when it does not contain the key in either spelling (a key can be shaped
like a request ID or even a type name); otherwise just the status. Never the
message. Every finished error message is then scanned once more: if fixed
wording around those fields, or any other local text, completes the key,
the message is emptied and the kind kept. Keys under 20 bytes are refused
at configuration, which keeps ordinary words out of that collision class. Without one (below), the same is followed by the unauthenticated
excerpt: at most 512 bytes of the body, without control characters. 408, 409, 429, 500, 502, 503, 504, 529, other
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

Credential-proxy mode. `AnthropicConfig::api_key` is optional. Without a
key the adapter sends no `x-api-key` or `Authorization` header: a
credential-injecting proxy on the host adds the key, so neither the harness
nor the VM it runs in holds it. The credential scan then has nothing to look
for and is skipped; it lives in the proxy, which holds the key and must
keep it out of what it relays. Plain `http` is accepted to any host in this
mode, and a keyless configuration whose host is `api.anthropic.com` is
refused (`run` says to pass `--api-key-env` or a proxy `--base-url`). With
a key, nothing changes: `https` unless the host is a loopback IP literal,
the key in `x-api-key`, and the scan.

Model attribution. A turn is credited to the model that served it, and
only fallback evidence explains a model other than the requested one.
Without a `fallback` block or a `fallback_message` iteration, on either
path, the response's `model` and every named `message` iteration must equal
the requested model exactly, or the response is `InvalidResponse`
("response model differs from the requested model without a fallback"),
before any tool runs. Captured live responses report the requested model
exactly.

Fallbacks are opt-in (`--fallbacks`). The harness exists to record, replay,
and evaluate specific models; a refusal the server silently reroutes to a
different model breaks model attribution and per-model comparisons, so by
default the request carries no `fallbacks` field and no beta header, and a
refusal ends the session as `ModelRefused`. Without fallbacks, a `fallback`
block or `fallback_message` iteration is `InvalidResponse`, and
`usage.iterations`, when present, must be `message` entries whose last one
matches the top level; when absent, the top level is used. Both shapes
come from live captures of `claude-opus-5-5`: with fallbacks on, usage
carried `iterations` with one `message` entry whose `model` was null; with
fallbacks off, usage was top-level only, with no `iterations`. So the
parser follows what the API was observed to send: with fallbacks on it
requires the array, and with fallbacks off it accepts the array's absence
and, should one appear, validates it like any other, since nothing
documents that the API never sends it there. With
`fallbacks: "default"`, a safety decline is rerun server-side on a suitable
model in the same call. The response then holds `fallback`
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
`sandbox_created` and `sandbox_recovered` count the create RPCs sent as
`create_attempts`; `sandbox_created` carries the create's `request_id` and
`session_finished` its `request_id_released` only when the session used them
(see [Create retries](#create-retries)), so a session against a daemon without
idempotent create writes neither.

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
`tool_executed`, `check_completed`, and `session_ended`. `session_started` records every
model-visible limit (steps, calls, timeouts, token budgets, argument and
final-answer bounds, and all tool limits); lifecycle windows are not recorded.
For a task with a seed or a check it also records `seed` (each file's path
and base64 contents) and `check` (the base64 script and `timeout_ms`), so a
replay is self-contained. Both are omitted when absent, so a journal without
them is byte-identical to one written before they existed, and the schema
stays 1. `check_completed` holds the check's outcome, its execution ID and at
most 4 KiB of each output stream, after the final `model_responded` and
before `session_ended`.

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

A recorded seed and check must pass the engine's own rules under the
recorded limits (valid base64 included). `check_completed` is allowed only
when a check was recorded, at most once, only right after an accepted
final answer in the last turn, and with neither output stream longer than
4 KiB of UTF-8; a `session_ended` with work outcome
`completed` requires it when a check was recorded.

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
journal. Replay has no expiry flags. It sends the idle timeout the recording
sent, from `sandbox_ready`, when there is one; otherwise the smallest whole
second, at least the 120-second default and at most the 1-hour ceiling,
that fits the recorded tool budget (`idle >= 1.5 * budget`). kilnd's clamp
and the tool-budget check then apply as for any session, so a replay that
cannot fit is refused before create. The largest recordable tool timeout,
10 minutes, needs 902 seconds. Since the model side is fixed, a divergence means kilnd returned
different tool results than during the recording.

Replay rebuilds the whole task from `session_started`, seed and check
included, seeds the fresh sandbox the same way, and reruns the check after
the replayed final answer. `Provider::check_completed` hands the result to
`ReplayProvider`, which compares it with the recorded `check_completed`:
the verdict and exit code must match, and any `error` equals any other
whatever its reason, since reasons carry timing detail. A mismatch sets
`check_diverged`, and the CLI prints `diverged at check` and exits non-zero.
Model-turn divergence is unchanged.

## Eval runner

`eval run` loads and validates every task before anything runs, refuses an
existing `--out`, reads kilnd's Health once for the manifest's feature list,
then creates `--out` with mode 0700 through the same descriptor walk as the
journal. Runs are sequential, one sandbox at a time, tasks in ID order and
trials 1 to N, each with a fresh provider. Each run's results line is built
from its journal (session ID, outcome, steps, tool calls, usage, check) and
appended to `results.jsonl` before the next run starts. With `--replay`, a
completed run is replayed against a fresh sandbox right after it. The suite
fails closed: after a run or replay whose sandbox deletion was not confirmed
(`cleanup_failed`, `Cleanup` or `CreateUncertain`), or replay outputs that
cannot be opened, it stops, writes `manifest.json`, `report.json` and
`report.md` for the runs so far, and exits non-zero. Cancellation stops it
after the current run's cleanup the same way. The manifest records the base
URL's host only, never a key, user, port, path or query. `eval selftest`
runs a bare and a solved session per task with scripted providers and fails
closed the same way.
