# Evals

A small suite for measuring models and harness changes on the same coding
tasks. Each task is a seeded workspace, a prompt, and a check script that runs
in the sandbox after the agent gives its final answer. The model never sees
the check.

## Task format

```text
evals/tasks/<id>/
  task.json      required
  prompt.md      required: the task, UTF-8, 1 byte to 64 KiB
  system.md      optional: a system prompt; otherwise the CLI's default
  check.sh       required: busybox sh, run as `/bin/sh -s` with the script on
                 stdin; exit 0 passes, anything else fails
  solution.sh    required: a reference solution, used only by `eval selftest`
  seed/          optional: every regular file under it is written to
                 /workspace/<same relative path> before the session
```

`task.json` takes exactly these fields:

```json
{
  "schema": 1,
  "id": "fix-off-by-one",
  "description": "For people; never shown to the model.",
  "check_timeout_secs": 30,
  "session_timeout_secs": 300,
  "max_steps": 30,
  "max_tool_calls": 60
}
```

- `id` matches `[a-z0-9][a-z0-9-]{0,63}` and equals the directory name.
- `check_timeout_secs` is 1 to 600 and at most half the session timeout. The
  check runs in the last `check_timeout_secs` of the session, so the model
  works in the rest.
- `session_timeout_secs`, `max_steps` and `max_tool_calls` are optional and
  override the command-line values for this task, within the usual ceilings.

The loader is strict. A task directory may hold only the entries above, and
only regular files and directories (a symlink, FIFO, socket or device
anywhere is refused). A seed has at most 256 files of at most 64 KiB each
(the file limit) and 4 MiB in total, with UTF-8 names that pass the same path
rules as the file tools. Empty directories in `seed/` are not recreated.
`check.sh` and `solution.sh` are 1 byte to 64 KiB of UTF-8, and no larger than
the tools' stdin limit. Every task in the directory loads and validates before
anything runs, even when `--task` picks a few, and every error names its task.

## Adding a task

1. Make `evals/tasks/<id>/` with the files above.
2. Write `check.sh` to start with `cd /workspace || exit 2`, and make it
   strict: compare exact bytes where you can (`cmp -s - file <<'END'`), and
   check that files the task must not change still hash the same
   (`sha256sum`). The sandbox is Alpine with busybox and no network.
   `/workspace` is the task's working tree; `/tmp` is writable scratch
   (`mktemp -d` works) that the check does not read.
   Redirect stdin from `/dev/null` when the check runs a script the agent
   wrote, since the check itself arrives on stdin.
3. Make sure the bare seed fails the check and `solution.sh` makes it pass.
   Commands, `solution.sh` included, start in `/workspace`; say so in
   `prompt.md` when the task depends on it.
4. Prove it against kilnd with `eval selftest --task <id>`.

## Self-test

```sh
cargo run --locked -- eval selftest --out evals-out/selftest
```

For each task this runs two sessions through the real engine and kilnd with
an in-process scripted model and no network provider (the solution is that
model's `exec` call, so it is in the session's request history but is never
sent to a model API): a bare one that answers at once, whose
check must fail, and one that runs `solution.sh` through the `exec` tool
(`/bin/sh -s` with the solution on stdin), whose check must pass. It prints
`ok <id>` or `FAIL <id>: <reason>` for each task and exits non-zero if any
fails. `--task ID` (repeatable) picks tasks, and `--tasks DIR` another suite.
Audits and journals go under `--out`, which must not exist.

## Running a suite

`eval run` takes every provider and limit flag `run` does, plus:

| Flag | Meaning |
|---|---|
| `--tasks DIR` | the suite, `evals/tasks` by default |
| `--task ID` | run only this task; repeatable; an unknown ID is an error |
| `--trials N` | runs per task, 1 to 100, default 1 |
| `--concurrency N` | runs at a time, 1 to 8, default 1 |
| `--max-run-secs N` | longest any run may take, 1 to 21600; caps every task's session timeout, its own included |
| `--out DIR` | required; must not exist; created with mode 0700 |
| `--label TEXT` | free text for the report, at most 128 bytes |
| `--replay` | replay each completed run's journal against a fresh sandbox |

```sh
cargo run --locked -- eval run --provider openai-compat \
  --base-url http://192.168.1.20:11434/v1 --model qwen3-coder \
  --trials 3 --replay --label "baseline" --out evals-out/qwen-baseline
```

`--out` is checked against the effective user running the command. Symlinks
in its existing part are resolved first. Every ancestor directory, from `/`
down, must be owned by root or that user, and may be group- or
world-writable only if it has the sticky bit (as `/tmp` has). The stricter
rule is for the immediate parent of `--out`: it must be owned by the
effective user and must not be group- or world-writable, sticky or not.
Missing parents are created with mode 0700, which satisfies this, and `--out`
itself is created new. So use a private parent and a child that does not
exist yet, such as `PRIVATE_DIR/out` where `PRIVATE_DIR` is mode 0700; a
sticky shared directory such as `/tmp` works only as an ancestor
(`/tmp/PRIVATE_DIR/out`), not as the parent (`/tmp/out`). Under `sudo`, where
the effective user is root, a path inside another user's home directory is
refused; use a root-owned private directory instead, such as one made by
`sudo mktemp -d`.

Runs start longest session timeout first (ties in task ID order) and
trials 1 to N, up to `--concurrency` at once, each with its own sandbox and
its own kilnd connection; a new run starts as soon as one ends.
`--max-run-secs` makes each run's session timeout the smaller of N and the
task's own (or `--session-timeout-secs`) without changing the start order;
it must be at least twice every task's check timeout, and a run that reaches it ends at the session deadline
and fails. The report notes the cap, and `eval compare` says when two
reports differ in it. Lines in `results.jsonl` are in completion
order, while the report lists tasks and trials in order. Each run holds at
most one command stream at a time, so 8 stays within kilnd's default cap on
streams per user; other harness sessions running as the same user at the
same time can make commands wait for a stream. A run's `wall_ms` includes
any wait for shared capacity, such as kilnd's or a provider's rate limits,
so compare wall times only between suites run at the same concurrency. Result
lines, journals and audits are written synchronously, so a stalled output
volume stalls every run in flight; keep `--out` on local storage.

Each run writes `OUT/runs/<id>/<trial>/audit.jsonl` and `journal.jsonl`
(and, with `--replay`, `replay-audit.jsonl` and `replay-journal.jsonl`), and
appends one line to `OUT/results.jsonl` as soon as it ends:

```json
{"task": "hello-file", "trial": 1, "session_id": "hs-…", "outcome": "completed",
 "work_outcome": "completed", "check": "passed", "check_exit_code": 0,
 "steps": 3, "tool_calls": 2, "input_tokens": 4210, "output_tokens": 180,
 "wall_ms": 9120, "replay": "matched", "error": null}
```

`check` is `passed`, `failed`, `error` (no verdict, such as a check cut off
by the session deadline) or `null` when the check did not run, because the
session did not end in an accepted final answer. `input_tokens` counts all
input, cached included; `cache_read_tokens` appears when some was read from a
prompt cache, and `cache_write_tokens` when some was written to one. `replay` is `matched`, `diverged_turn:<n>`, `diverged_check`,
`failed`, or `null` without `--replay` or for a run that did not complete.
In a priced suite, `cost_micro_usd` is the run's estimated cost in US
micro-dollars (0 for a run that never reached the model), and
`long_prompt_requests` appears when some responses were priced at a
long-prompt rate.

At the end it writes `OUT/manifest.json` (harness version, label, provider
kind and model, the base URL's host only, tasks, trials, runs at a time,
the run cap when set, the effort sent and the thinking clearing (`off` or
`keep N`, Anthropic only), the output limit per
response, the price table and rates used or why the runs are unpriced,
start and end times, and the features kilnd listed), `OUT/report.json`, and `OUT/report.md`: per
task the runs, passes, pass rate, outcome counts, mean and median steps, tool
calls and wall time, mean tokens, mean cost, and replay counts; overall the macro pass
rate (the mean of the task pass rates), pass@1 with the number of trials, and
totals and means, with the total cost and the cost per passed run.

### Cost

Cost is an estimate from the usage each model response reported, at
Anthropic's list prices for Claude Fable 5.2, Opus 5.5, Sonnet 5.5 and
Haiku 5.5 (the table of 2026-11-13, pinned with its rates in the manifest).
It is not billing and never a limit; the token, byte and time limits are
the controls. Each response is priced on its own: a Claude Haiku 5.5 request
whose prompt, counting uncached input, cache reads and cache writes, is over
100,000 tokens pays the long-prompt rates on every category, output
included, and the report counts those responses. Cache writes are priced at
the 5-minute rate, the only cache the harness asks for. The report's header
gives the model's context window and output limit with the table's date.

A suite is unpriced, and `report.md` says `Cost: unpriced` with the reason,
for openai-compat, a model the table does not list, or `--fallbacks`, since
another model may then serve, and bill, a turn. A task's or the suite's cost
is shown only when every run in it was priced; an unknown cost is never
counted as zero. For a listed model, `run` and `eval run` refuse a
`--max-output-tokens` above its output limit of 128,000 before anything
runs.

```sh
cargo run --locked -- eval cost evals-out/haiku-hard
```

prices a finished run again from its journals with the current price table
and prints Markdown: per task the runs, passes, cost and long-prompt
responses, then the total and the cost per passed run. It assumes every turn
was served by the requested model. Each journal is verified, hash chain
included, before it is priced, and a missing or altered journal fails the
whole command rather than leaving a partial total. Like `eval compare`, it
reads files only and never contacts kilnd; it refuses an openai-compat run
or a model without prices.

The suite fails closed. After a run whose sandbox deletion was not confirmed
(outcome `cleanup_failed`, or a `Cleanup` or `CreateUncertain` error,
including its replay's) it stops: no new run starts, the other runs in flight
are canceled, and it waits for each of them to finish its own cleanup and
write its line (usually with outcome `canceled`). It then writes the report
for the runs so far and exits non-zero. A provider that cannot be built, a
connection that cannot be opened, a run's or replay's audit, journal or
result line that cannot be written (a run that stops the suite is not
replayed), or progress output that fails, such as a closed stdout pipe, stop
it the same way, and Ctrl-C does too, with the same partial report and a
non-zero exit. Every reason is kept, in the order seen. Otherwise it exits 0
whatever the pass rate.

## Comparing runs

```sh
cargo run --locked -- eval compare evals-out/qwen-baseline evals-out/qwen-new-prompt
```

prints Markdown: for every task in either report, sorted, the pass rate, mean
steps and mean total tokens from base to new with their deltas, tasks present
on one side only marked as such, and an overall line. When both reports are
priced, it adds the total cost and the cost per passed run from base to new.
It adds a note when
the two reports ran at different concurrency, run caps, effort, thinking
clearing or output limits per response, or were priced with different price
tables; reports written before these were
recorded carry none of them, and an unknown setting is not reported as a
difference.

## The starter suite

Eight busybox-only tasks: `hello-file` (exact file contents), `fix-off-by-one`
(a loop bound), `count-errors` (count `ERROR`-level log lines, with
distractors), `csv-sort` (numeric sort with a tie-breaker), `rename-ext`
(rename four files, contents unchanged), `config-edit` (change one setting,
every other line identical), `fix-failing-test` (fix a library so an
unchanged test passes) and `dedupe` (remove duplicate lines in place, in
order).

## The hard suite

`evals/hard` holds twelve harder tasks in the same format, for telling
configurations apart once the starter suite passes. All of them use the
default system prompt. The first eight below give a session 900 seconds, 40
steps and 80 tool calls, with a 60-second check. The last four are larger
(an existing program to investigate, or a long written spec whose rules
interact) and give a session 1500 seconds, 60 steps and 120 tool calls, with
a 120-second check that runs at least 15 hidden cases. The "write a script"
tasks state a full contract, in the prompt or in a seeded document, and
their checks run the script on many inputs that are not in the seed or the
prompt, covering every rule the contract states; a check prints at most one
short line, such as `case 3`, naming the first failing case.

- `semver-sort`: sort versions by SemVer 2.0.0 precedence, with validation,
  numbers too long for floating point and a stable order.
- `csv-colsum`: sum one column of an RFC 4180 file, with quoted fields,
  CRLF, embedded newlines and exact exit codes.
- `build-order`: order build targets so dependencies come first, taking the
  smallest ready name each time, and report cycles and syntax errors.
- `strutil-spec`: make a shell string library match its written spec when
  its tests catch only two of its four bugs.
- `log-rollup`: count log levels per UTC hour across files, with offsets,
  calendar validation and malformed-line counting.
- `rename-function`: rename a shell function across a project without
  touching look-alike names or the changelog.
- `ini-merge`: merge layered INI files, with case-insensitive names, unset
  markers and first-appearance order.
- `orders-report`: produce a sales report by its written rules, over older
  notes that contradict two of them.
- `ledger-triage`: fix the three reported bugs in an existing awk ledger
  program, and find the two unreported ones by reading the code against its
  manual.
- `stak-interp`: write an interpreter for a small stack language from its
  spec: control flow, early-binding definitions, static and runtime errors
  with line numbers, and a 200,000-iteration loop within 10 seconds.
- `ignore-match`: decide which paths an ignore-rules file ignores, with
  anchoring, `**`, classes, directory-only rules, last-match negation and
  excluded ancestors.
- `patch-apply`: apply a unified diff all or nothing, with an offset search
  whose result carries to later hunks, missing-newline markers, CRLF lines,
  creation and deletion.

Run it with `--tasks evals/hard`, for example
`cargo run --locked -- eval run --tasks evals/hard ... --out DIR`, and
self-test it with `eval selftest --tasks evals/hard`. The starter suite stays
the smoke tier and the default.

Every hard check and solution is POSIX `sh` and POSIX `awk`, so the tests run
them on the host too. `tests/fixtures/eval-hard/<id>/` holds overlays for each
task: `fail-*` directories are plausible wrong answers the check must refuse,
and `pass-*` directories correct answers written differently from the
reference solution, which it must accept.

## Honest limits

- The check runs in the sandbox the agent used. A model that tampers with the
  workspace, replaces tools it can write to, or leaves processes running can
  influence its own check. Evals measure; they do not prove.
- A check is a finite script that tests behaviour on the inputs it tries. An
  answer that special-cases those inputs can still pass, so a check samples
  more inputs than the prompt names and may add a cheap source check (for
  example, `fix-off-by-one` requires `sum.sh` to keep a loop), but no check
  proves an answer correct.
- Pass rates over few trials are noisy. Reports state the number of trials;
  compare runs with the same N, and prefer more trials before trusting a
  small difference.
- Replay answers every model request from the journal, so a replay divergence
  isolates nondeterminism in the environment (kilnd, the sandbox, timing), not
  model quality. For example, seed files carry the time they were written, so a
  recorded `ls -l` of the workspace diverges when the replay runs in a later
  minute.
