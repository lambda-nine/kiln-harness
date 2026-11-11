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
   (`sha256sum`). The sandbox is Alpine with busybox, no network, and
   `/workspace` is the only writable path, so do not write anywhere else.
   Redirect stdin from `/dev/null` when the check runs a script the agent
   wrote, since the check itself arrives on stdin.
3. Make sure the bare seed fails the check and `solution.sh` makes it pass.
   Commands run from `/`, so the solution uses absolute paths or
   `cd /workspace`.
4. Prove it against kilnd with `eval selftest --task <id>`.

## Self-test

```sh
cargo run --locked -- eval selftest --out evals-out/selftest
```

For each task this runs two sessions through the real engine and kilnd with
a scripted model and no model API: a bare one that answers at once, whose
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
| `--out DIR` | required; must not exist; created with mode 0700 |
| `--label TEXT` | free text for the report, at most 128 bytes |
| `--replay` | replay each completed run's journal against a fresh sandbox |

```sh
cargo run --locked -- eval run --provider openai-compat \
  --base-url http://192.168.1.20:11434/v1 --model qwen3-coder \
  --trials 3 --replay --label "baseline" --out evals-out/qwen-baseline
```

Runs are sequential, one sandbox at a time, tasks in ID order and trials 1 to
N. Each run writes `OUT/runs/<id>/<trial>/audit.jsonl` and `journal.jsonl`
(and, with `--replay`, `replay-audit.jsonl` and `replay-journal.jsonl`), and
appends one line to `OUT/results.jsonl`:

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
prompt cache. `replay` is `matched`, `diverged_turn:<n>`, `diverged_check`,
`failed`, or `null` without `--replay` or for a run that did not complete.

At the end it writes `OUT/manifest.json` (harness version, label, provider
kind and model, the base URL's host only, tasks, trials, start and end times,
and the features kilnd listed), `OUT/report.json`, and `OUT/report.md`: per
task the runs, passes, pass rate, outcome counts, mean and median steps, tool
calls and wall time, mean tokens, and replay counts; overall the macro pass
rate (the mean of the task pass rates), pass@1 with the number of trials, and
totals and means.

The suite fails closed. After a run whose sandbox deletion was not confirmed
(outcome `cleanup_failed`, or a `Cleanup` or `CreateUncertain` error,
including its replay's) it stops, writes the report for the runs so far, and
exits non-zero. Ctrl-C stops it after the current run's cleanup, with the same
partial report and a non-zero exit. Otherwise it exits 0 whatever the pass
rate.

## Comparing runs

```sh
cargo run --locked -- eval compare evals-out/qwen-baseline evals-out/qwen-new-prompt
```

prints Markdown: for every task in either report, sorted, the pass rate, mean
steps and mean total tokens from base to new with their deltas, tasks present
on one side only marked as such, and an overall line.

## The starter suite

Eight busybox-only tasks: `hello-file` (exact file contents), `fix-off-by-one`
(a loop bound), `count-errors` (count `ERROR`-level log lines, with
distractors), `csv-sort` (numeric sort with a tie-breaker), `rename-ext`
(rename four files, contents unchanged), `config-edit` (change one setting,
every other line identical), `fix-failing-test` (fix a library so an
unchanged test passes) and `dedupe` (remove duplicate lines in place, in
order).

## Honest limits

- The check runs in the sandbox the agent used. A model that tampers with the
  workspace, replaces tools it can write to, or leaves processes running can
  influence its own check. Evals measure; they do not prove.
- Pass rates over few trials are noisy. Reports state the number of trials;
  compare runs with the same N, and prefer more trials before trusting a
  small difference.
- Replay answers every model request from the journal, so a replay divergence
  isolates nondeterminism in the environment (kilnd, the sandbox, timing), not
  model quality.
