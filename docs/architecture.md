# Architecture

```text
scripted provider (live adapter later)
        │ typed ModelAction
        ▼
agent loop ── hard steps/calls/session deadline ── append-only audit
        │ typed ToolCall                               │
        ▼                                              └─ hashes, sizes, IDs only
tool registry ── strict schemas and byte/time bounds
        │ literal argv via kilnd.v1
        ▼
kilnd Unix socket ── one sandbox ── bounded executions
```

`Provider` and `SandboxApi` are separate traits. The first emits a typed tool
call or final answer; the second represents only kilnd API semantics. The real
adapter uses gRPC over the configured Unix socket. Tests use an in-memory fake.
No harness code depends on kiln files, process layout, or CLI behavior.

## Session lifecycle

1. Open the audit and calculate one absolute session deadline.
2. Create exactly one sandbox with network none, 256 MiB memory, 0.5 CPU,
   64 PIDs, 256 MiB disk, read-only root, and writable `/workspace`.
3. Capture a bounded `/workspace` file-set baseline through kilnd.
4. Alternate provider turns and typed tool results until a final answer or a
   budget/cancellation/error ends work.
5. Retry idempotent `DeleteSandbox` and verify absence after ambiguous errors.
   Cleanup has a separate 10-second wall window because kilnd may need to drain
   descendants and publish terminal state after the work deadline. Failure to
   confirm deletion is returned as a cleanup error.

Create shares the 30-second session wall deadline. Every request has unique
`owner` and `session` labels. If its response is lost or times out, the harness
polls bounded non-deleted listings for those labels and deletes every match.
If an ambiguous failure produces no discoverable row before the recovery
window closes, the result is `CreateUncertain`, not a false cleanup claim.
Idempotent create and sandbox TTLs remain control-plane roadmap items. Forced
process death still depends on kilnd reconciliation and a future TTL.

## Tool mapping

| Tool | kilnd executions | Validation |
|---|---:|---|
| `exec` | 1 | 1–128 literal UTF-8 argv entries, no NUL, 32 KiB argv, 64 KiB stdin, at most 5 s |
| `read_file` | 2 | relative `/workspace` path; fixed `stat` then bounded `head`; at most 64 KiB returned |
| `write_file` | 1 | relative `/workspace` path; fixed `dd` argv and bounded stdin; at most 64 KiB |
| `list_dir` | 1 | relative `/workspace` directory; fixed `ls` argv |
| `workspace_diff` | 1 | fixed NUL-delimited `find`; compares at most 512 paths / 64 KiB with baseline |

No model string is interpolated into a host command. Fixed helper programs and
validated model values are separate sandbox argv entries. A model can request
`sh -c` only as explicit sandbox argv through `exec`; network remains disabled.

Defaults are eight model steps, six tool calls, a 30-second session wall
deadline, a 5-second tool wall deadline, a 10-second lifecycle cleanup window,
64 KiB model arguments, and 128 KiB encoded tool results. A tool reserves 250
ms of its wall budget for kilnd settlement; exceeding the wall deadline drops
the RPC to request cancellation. kilnd independently bounds retained output
and owns execution cleanup. Configured lifecycle windows are rejected above 60
seconds so deadline construction cannot overflow after a sandbox exists.

## Audit

Each JSONL record has a session-local sequence, timestamp, session ID, sandbox
ID once known, and one event. Accepted model arguments and final answers, tool results, and
sandbox output are represented by SHA-256 plus byte counts or by output
evidence; their contents are excluded. Tool events retain safe call IDs,
registered tool names, kilnd execution IDs, and the sandbox ID. Unknown model
tool names are recorded as `unknown`, not copied into the log. Oversized or
invalid model actions are recorded as rejected before termination. On a client
deadline/cancel, the harness queries recent sandbox executions to recover any
ID kilnd already assigned and copies that row's byte/truncation evidence; an
empty ID list with unknown totals means no row was observable through v1 within
the bounded recovery query. Baseline execution failures use the same recovery.
