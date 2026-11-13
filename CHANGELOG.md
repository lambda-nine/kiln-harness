# Changelog

Notable changes to kiln-harness. Versions follow
[Semantic Versioning](https://semver.org/); before 1.0, any minor release may
change the CLI or audit format.

## Unreleased

- `eval run --max-run-secs N` (1 to 21600) caps every run's session timeout
  at N seconds, sandbox creation and the check included, even for tasks
  whose `task.json` sets a longer one; it is refused before anything runs
  when it is under twice any task's check timeout. A run that reaches it
  ends at the session deadline and fails. Runs now start longest session
  timeout first (ties in task order, trials 1 to N), using each task's own
  timeout even under the cap, so long runs no longer start last; the report still lists tasks in order. `manifest.json` and
  `report.json` gain `max_run_secs` (absent when unset and in older files),
  `report.md` prints the longest run allowed, and `eval compare` notes when
  two reports differ in it.
- `eval run --concurrency N` (1 to 8, default 1) runs up to N runs at once,
  each with its own sandbox and its own kilnd connection, and starts the
  next as soon as one ends; one at a time behaves as before. Lines in
  `results.jsonl` are written in completion order while the report keeps
  task and trial order, and any stop cancels the other runs in flight and
  waits for their cleanup before the report is written; audit, journal and
  progress output failures stop it too, and a run that stops it is not
  replayed. Session discovery retries kilnd's Health admission refusals
  within the session deadline. `manifest.json` and
  `report.json` gain `concurrency` (a file without it reads as 1),
  `report.md` prints the runs at a time, and `eval compare` notes when two
  reports differ in it, since their wall times are then not directly
  comparable.
- Eval prompts match the execution context: the 13 task prompts that said
  commands start in `/` (every `evals/hard` task and `fix-failing-test`) now
  say they start in `/workspace`, with examples that use relative paths.
  Checks, seeds and solutions are unchanged, but results from these prompts
  are not directly comparable with runs made before this change.
- kilnd API milestone 4: the model's `exec` runs through `ExecStream`. Every
  live session uses a third execution context, `workspace_stream_v1`
  (`/workspace` and the fixed env, as `workspace_v1`), journaled in
  `session_started.exec_context`; the file tools, seeding, the baseline
  manifest and the check stay unary. Discovery also requires
  `FEATURE_EXEC_STREAM` and `ApiLimits.max_stream_timeout_ms` (now kept in
  `ApiLimits`, with `default_stream_timeout_ms`) of at least the tool
  timeout, refusing otherwise with `FAILED_PRECONDITION` before anything is
  created; there is no unary fallback. Replay keeps the recorded context:
  `workspace_v1` and legacy journals replay unary, against a daemon with or
  without streaming, and stay byte-identical. A command is stopped by
  dropping its stream, which kilnd cancels, as soon as its delivered chunks
  and reported gaps show more than `tools::RUNAWAY_OUTPUT_BYTES` (8 MiB) on
  either stream; its final row is read back, the result gains
  `"stopped": "output_limit"`, and the session goes on. Live output is
  lossy, so a command whose excess output was dropped live and that then
  goes quiet is bounded by its timeout instead. A stream that ends without
  its result after `ExecStarted` is followed to kilnd's row with
  `GetExecution` within the call's time and recovery window, and gives the
  result `ExecFinished` would have; a row never read as terminal ends the
  session with the ID kept and a fixed message. A stream that breaks the
  protocol ends the session as unusable with a fixed message and the ID, if
  one arrived, and its row is never used as a result. A stream that breaks
  before `ExecStarted` is treated as a lost response; recovery after a
  streamed call follows its started execution by ID even when the call was
  canceled or hit its deadline, keeps that ID if a cancellation cuts it
  short, and reads every row it discovers with `GetExecution` and checks it
  as a returned streamed row. Seed executions are known before the first
  model call, so recovery never takes a seed row for a model call's. A
  typed
  `exec_stream_limit` refusal is retried with backoff (50 ms doubling to
  1 s) within the call's budget and is otherwise an ordinary tool error; a
  typed `backend_unsupported:exec_stream` ends the session as
  sandbox-unusable through `api::sandbox_unusable_refusal`. A streamed row
  must say it was streamed (`Execution.mode`, new) and must hold output
  wherever kilnd observed some or the live stream delivered some (kilnd's
  `--output-storage full`), or the session ends at that call with the
  execution ID kept and its output withheld; a command whose output was all
  lost live is not caught this way, and an `INTERRUPTED` row, which kilnd's
  restart reconciliation leaves without output, is exempt. `SandboxApi::exec_stream` (new, with
  `StreamObserver`, `StreamedExec`, `LiveStream`, `LiveDelivery`) checks the
  stream protocol (`seq`, started first, nothing after finished, matching
  IDs, named streams, per-stream offsets with no overlap or unreported
  hole, and `ExecFinished`'s live totals). The new `live::LiveOutput` sink
  receives live bytes on the host (`Harness::with_live_output`) through a
  bounded queue and a writer thread of its own, so a slow sink never holds
  up a command, a cancellation or cleanup; bytes that do not fit reach it
  as a gap, and `Harness::drain_live_output` lets the last of them through
  (`Duration::MAX` waits with no deadline).
  `demo`, `run` and `replay` gain `--live`, which prints them to stderr
  sanitized. The audit's `tool_completed` gains an optional `live`
  (delivery totals and `stopped`), omitted for unary calls. In a streaming
  session the `exec` tool's description adds that a command whose stdout or
  stderr passes 8 MiB is stopped; recordings of earlier contexts keep their
  tool definitions. Journal schema 1, unchanged shape.
- kilnd API milestone 3b (kilnd `0c79944`, whose `kilnd.v1` schema is now
  vendored, ExecStream messages included; the harness does not stream).
  Every execution (a model's `exec`, the file tools, the workspace
  manifest, seeding and the task check) sends `cwd` `/workspace` and three
  fixed env overrides, `TERM=dumb`, `NO_COLOR=1`, `PAGER=cat`, over kiln's
  fixed `HOME=/root`, `PATH` and `TERM`. `SandboxExec` gains `cwd` and `env`
  (`api::EnvVar`). A session refuses a daemon that does not list both
  `FEATURE_EXEC_CWD` and `FEATURE_EXEC_ENV`, or whose `max_cwd_bytes`,
  `max_env_entries`, `max_env_name_bytes`, `max_env_value_bytes` or
  `max_env_total_bytes` (now kept in `ApiLimits`) cannot fit them
  (`FAILED_PRECONDITION`, before anything is created or recorded). The
  default system prompt says commands run in `/workspace` and relative
  paths refer to workspace files. A typed `execution_start_raced` Exec
  refusal is sent again once within the same budget, and a second one is an
  ordinary tool error. `sandbox_not_ready` from an Exec is now terminal with
  no `GetSandbox` lookup, and `SandboxApi::sandbox_ready` is removed. A typed
  `backend_unsupported:exec_cwd`/`:exec_env` or
  `sandbox_unsupported:exec_cwd`/`:exec_env` refusal ends the session as
  sandbox-unusable, at seeding, the baseline, a tool or the check alike
  (`api::sandbox_unusable_refusal`). `Execution` gains `cwd`, `env_names`
  and `env_sha256`; every returned or recovered execution, and every
  finished reusable row first seen by the final check, must show what
  was sent (`api::env_sha256` is kilnd's digest), and one that does not ends
  the session ("kilnd ran an execution with a different cwd or env than
  requested") with its ID kept and its output withheld. The journal keeps
  schema 1: `session_started` gains `exec_context`, always `workspace_v1`
  for new sessions; absent, as in every older journal, means the legacy
  context (no cwd, no env), so old journals are byte-identical. The new
  `Harness::replay` runs a `ReplayProvider`'s recorded task in its recorded
  context, so legacy journals replay as they ran; the public `Harness::run`
  always uses `workspace_v1`, and the context type is crate-private.
  `ToolRegistry::start_session` is now crate-private. The `run` and
  `replay` CLI paths and the eval runner use `Harness::replay` for
  replays. No audit schema change.
- The tested kiln pin moves to `8735907`, which `scripts/integration-vm.sh`
  requires, and the minimum to `5cada84`, the first kiln with exec options.
- An eval suite. `SessionTask` gains an optional `seed` (files written into
  `/workspace` with the `write_file` Exec before the baseline; a failure is
  `HarnessError::Seed`) and `check` (a script run once as `/bin/sh -s` after
  an accepted final answer, in the last `check.timeout` of the session, which
  the model's work deadline leaves free). `SessionOutcome::check` carries
  its `CheckOutcome`, which never changes the work outcome. The journal keeps
  schema 1: `session_started` gains `seed` and `check` only when present, and
  a new `check_completed` record follows the final answer; the grammar checks
  both. Replay seeds and checks the fresh sandbox and reports
  `diverged at check`. The audit adds `workspace_seeded` and a metadata-only
  `check_completed`. New commands: `eval run` (shares `run`'s provider and
  limit flags), `eval compare` and `eval selftest`, with eight starter tasks
  in `evals/tasks/`. `ToolRegistry::start_session` takes the seed, and
  `Provider` gains a default `check_completed` hook.
- First eval-suite review round. Cancellation wins at the task check as it
  does at the final usability check: a token already canceled sends no
  check, the wait is biased toward it, and one canceled by the time the
  check answers turns its verdict into `canceled` (the execution and output
  are kept). Check output is decoded as lossy UTF-8 before it is cut to
  4 KiB at a character boundary, and the journal grammar rejects a
  `check_completed` stream longer than 4 KiB. `fix-off-by-one` checks ten
  inputs and requires `sum.sh` to keep a loop, so a lookup of the named
  values no longer passes. `eval selftest` refuses an empty suite like
  `eval run`.
- Second eval-suite review round. The `fix-off-by-one` loop check accepts
  every POSIX loop: `until` as well as `while` and `for`, and a keyword alone
  at the end of its line.
- First review round for idempotent create. Cleanup releases the request ID
  only when the create took a single attempt: after a retry an abandoned
  attempt may still reach kilnd, so the key is kept, every delete sends
  `release_request_id = false`, `request_id_released` stays absent, and a
  late attempt is answered from the retained `DELETED` row instead of
  creating a sandbox. Cancellation now wins every tie in the retry phase:
  each wait polls it first, it is checked again after every Create and
  Health answer before the attempt cap, window, deadline or retry rules can
  end the phase, and an attempt is counted only once its RPC is sent. The
  README and threat model no longer promise that a daemon replaced by an
  older build never receives a retry: the Health reread cannot be bound to
  the Create after it, so a replacement in that sub-second gap can make a
  brief duplicate, which the echo check stops and label recovery deletes.
- kilnd API milestone 2 (kilnd `4baf986`, whose `kilnd.v1` schema is now
  vendored): idempotent create. When a serving daemon lists
  `FEATURE_IDEMPOTENT_CREATE`, the session's create carries the request ID
  `kiln-harness:<session ID>` and is retried with an equal request after
  `UNAVAILABLE`, `UNKNOWN`, `DEADLINE_EXCEEDED`, `ABORTED`, `INTERNAL` or
  `CANCELLED`: at most 8 attempts, backoff from `lifecycle_retry_delay`
  doubling to 2 s, within the lifecycle window, and only after a re-read
  Health still lists the feature. Attempts have no client timeout. A create
  answered `OK` but not `READY`, or echoing another request ID, is recovered
  by its labels and fails `create_failed`. With `FEATURE_REQUEST_ID_RELEASE`,
  the cleanup after a successful create releases the key, repeating the
  delete after absence if needed; release never fails cleanup. The audit
  adds `request_id` and `create_attempts` to `sandbox_created`,
  `create_attempts` to `sandbox_recovered`, and `request_id_released` to
  `session_finished`; older records still parse, and the journal is
  unchanged. `SandboxApi::delete` takes `release_request_id` and returns
  `Deleted`; `Sandbox` gains `state`, `failure_reason`, `request_id` and
  `request_id_released`; `SandboxRequest` gains `request_id`; `Feature`
  gains `RequestIdRelease`.
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
