# kiln-harness

A provider-neutral agent loop for running tool calls in
[kiln](https://github.com/lambda-nine/kiln) sandboxes. It talks only to
[kilnd](https://github.com/lambda-nine/kilnd)'s `kilnd.v1` gRPC API over a Unix
socket: one no-network sandbox per session, five typed tools, multi-call model
turns, hard budgets, a metadata-only audit log, a content-bearing session
journal that replays deterministically, and verified sandbox deletion after
every session.

**Status: experimental.** Besides a deterministic scripted provider, which
needs no model account, API key or network access, it has one live provider:
an adapter for the OpenAI-compatible `/v1/chat/completions` API that the MLX
server, Ollama, llama.cpp, and vLLM speak, for running local models. kiln is
experimental shared-kernel isolation and **not a boundary for hostile
multi-tenant workloads**. Report
vulnerabilities privately as described in [SECURITY.md](SECURITY.md).
Licensed under [Apache-2.0](LICENSE).

## Lambda IX repositories

| Repository | What it is |
|------------|------------|
| [kiln](https://github.com/lambda-nine/kiln) | Shared-kernel Linux sandbox runtime (namespaces, cgroups v2, seccomp, Landlock, nftables) |
| [kilnd](https://github.com/lambda-nine/kilnd) | Per-node control daemon with a gRPC API; drives kiln today, with an experimental Firecracker microVM backend that is disabled for production |
| [kiln-guest](https://github.com/lambda-nine/kiln-guest) | Build scripts for the Firecracker guest image that runs kiln inside a microVM ("VM outside, kiln inside") |
| **kiln-harness** (this repository) | The agent loop on top of kilnd's API |

## Quickstart

Host-side tests use a fake kilnd and a local fake model server, and run on
Linux or macOS with rustup (`rust-toolchain.toml` selects Rust 1.88), `protoc`
(`sudo apt-get install protobuf-compiler` or `brew install protobuf`), and a C
compiler for the `ring` TLS crate (`build-essential` on Ubuntu; the Xcode
command line tools on macOS):

```sh
git clone https://github.com/lambda-nine/kiln-harness
cd kiln-harness
cargo test --locked
```

The live demo needs a running kilnd. Set one up first by following kilnd's
quick start (Linux 6.8+ with cgroups v2, or the documented Lima VM on a Mac),
then, inside that machine:

```sh
cargo run --locked -- demo --audit audit/demo.jsonl
```

## Overview

The smallest useful provider-neutral agent loop over `kilnd.v1`. It creates one no-network sandbox per session, exposes five typed tools
(`exec`, `read_file`, `write_file`, `list_dir`, and `workspace_diff`), runs
model turns that may request several tool calls at once, applies hard
step/call/per-turn/token/input/output/deadline budgets, writes a metadata-only
append-only JSONL audit with host-generated call correlation IDs, records a
hash-chained session journal with the content replay needs, and retries sandbox
deletion after every returned work outcome. Every delete acknowledgement needs
a separate listing confirming absence within the cleanup window.
An unconfirmed cleanup is a bounded, explicit error rather than a hidden
success. Typed gRPC status codes distinguish definite create rejections from
ambiguous outcomes, which require lifecycle recovery. All sandbox work goes
through kilnd's Unix-socket API; the harness never invokes `kiln` or a host shell.

## Pinned control plane

- kiln behavior and persistent-exec v1: `f21ea88115fdcd10c5a3689832199d1f79c50d4b`
- kilnd API and runtime semantics: `867d731162d4581c01c756036873732c42649c6f`
  (parent `2c49761a3206c29c01b21bd9489e2b946263304c`)

The compatible `kilnd.v1` protobuf schema is vendored under `proto/` so this
repository builds against the reviewed API even if a sibling checkout is on a
different branch.

## Deterministic demo

Run this inside the Linux VM where `/run/kilnd/kilnd.sock` exists:

```sh
cargo run --locked -- demo --audit audit/demo.jsonl
```

The script takes five model turns. It writes `/workspace/demo.txt`; reads it
back and lists the workspace in one two-call turn; checks the session file-set
diff; runs `wc` with literal argv; then answers. The scripted provider verifies
every result before its next turn. The command prints the journal path, by
default `journal/demo-<unix_ms>.jsonl`; pass `--journal PATH` to choose one.
Journals are never overwritten, and their directory must be owned by the user
running the harness and writable by no one else. Ctrl-C and wall deadlines stop
waiting on an active RPC, including sandbox create and the workspace baseline;
kilnd is asked to cancel it, then deletion is retried and verified within a
separate bounded cleanup window.

## Running against a local model

`run` carries out one task with a real model through any server that speaks
the OpenAI-compatible chat-completions API with tool calling. The harness runs
inside the Linux VM next to kilnd, so `HOST` below is the Mac's LAN address
or the VM's address for its host (on Lima, `host.lima.internal`), not
`localhost`. The model server must listen on that interface, not only on
loopback.

With Ollama (`OLLAMA_HOST=0.0.0.0 ollama serve` on the Mac):

```sh
cargo run --locked -- run --provider openai-compat \
  --base-url http://HOST:11434/v1 --model qwen3:8b \
  --prompt "Write a Python script that prints the first ten primes, run it, and report the output."
```

With MLX (`mlx_lm.server --host 0.0.0.0 --port 8080 --model <model>` on the
Mac):

```sh
cargo run --locked -- run --provider openai-compat \
  --base-url http://HOST:8080/v1 --model <model> \
  --prompt-file task.txt
```

The model must support tool calling through the API. Small local models do
better when the prompt says to use the tools; otherwise some answer from
memory, which the harness accepts as a final answer. `--system-file`
replaces the default system prompt, which describes the sandbox and the
tools. Limits default to 40 steps, 120 tool calls (8 per turn), a 900-second
session, a 30-second tool timeout, 16,384 output tokens per request, and
2,000,000 input and 600,000 output tokens per session; each has a flag, and
the hard ceilings in `docs/architecture.md` still apply. Reasoning tokens
count toward the output budgets: a thinking model such as Qwen3 can spend
thousands of hidden tokens per turn before it answers, and with too small a
cap the turn is truncated and the session fails. Lower `--max-output-tokens`
only for models that do not think. `--temperature`
(default 0) and `--seed` are forwarded. Output matches `demo`. The audit
defaults to `audit/run.jsonl` and the journal to
`journal/run-<unix_ms>.jsonl`, which `replay` reruns without the model
server.

A server that needs a key gets it from an environment variable named by
`--api-key-env VAR`, never from a flag. The key is sent only in the
`Authorization` header. With a key, error bodies are never quoted, and a
response that contains the key, exactly or in its JSON-escaped spelling, at
any JSON layer the protocol defines (the body, and the JSON nested inside
each tool call's `function.arguments` string, whatever its type) is refused
whole. So the harness never writes the key or its JSON-escaped form to the
journal, the audit, or an error. A server holding the key could still send
it back transformed or split, which cannot be detected. Plain `http` is
accepted for a model server on the LAN without a key; with a key, the base
URL must be `https` unless its host is a loopback IP address.

Responses must report plausible token `usage`, or the session fails: budgets
cannot be enforced without it. Usage accuracy is the server's job and is
trusted for cost only; the harness also counts model output itself and stops
a session past `--max-total-output-bytes` (8 MiB by default) whatever usage
says. Response payloads are capped at 4 MiB and requests at 8 MiB; rate
limits, overload, and transport failures are retried up to three attempts
with jittered backoff, honoring `Retry-After`. `--prompt-file` and
`--system-file` must be regular files; a symlink as the last path component
is refused. The final answer is printed with control characters escaped.

## Journal and replay

The audit stays metadata-only. The journal is its content-bearing counterpart:
the task, each request's SHA-256, every model response, and every tool call and
result, one hash-chained JSONL record each. It is a trusted 0600 host file.

```sh
cargo run --locked -- journal verify journal/demo-<unix_ms>.jsonl
cargo run --locked -- replay --from journal/demo-<unix_ms>.jsonl --audit audit/replay.jsonl
```

`journal verify` runs offline. It checks size limits, the hash chain, the
recorded limits against the hard ceilings, and the event grammar: pairing,
order, each tool record against the model's sanitized call, each request's
message count and hash against the history rebuilt from the records, and a
terminal record consistent with the session. It then prints the session,
record count, outcome, whether the sandbox was deleted, turns, tool calls, and
token usage. A session whose sandbox deletion was not confirmed ends
`cleanup_failed`.

`replay` reruns the recorded task under every recorded model-visible limit in
a fresh sandbox. It answers each model request from the journal after
checking that the request, including its output-token cap, hashes the same as
the recorded one, and compares each tool call and result with the recording as
it completes. Before contacting kilnd it opens its outputs and refuses if any
of them is the same open file as the source. It prints `reproduced` only when a
completed recording replays to the same final answer with every turn and tool
call consumed. Otherwise it prints `diverged at turn N`, the first turn where
a tool result or a request differed because the sandbox answered differently,
and exits non-zero.

Journal and audit directories follow ssh's `StrictModes`: the output's own
directory must belong to you and be writable by no one else, and every
directory above it must belong to you or root, be readable and searchable by
you, and be sticky if others can write it. On systems with a
user-private-group umask of `0002`, such as Ubuntu, a checkout can be
group-writable and is then refused as an ancestor. The error names the
directory and its mode. Fix it with `chmod g-w <dir>`, or pass `--journal` or
`--audit` a path under a private directory such as one made with
`mkdir -m 700`. Directories the harness creates are always 0700 and its files
0600, whatever the umask, and a failed or refused open removes what it
created. On Linux, POSIX ACL entries show in the mode bits and are checked
with them. On macOS the directory rule checks ownership and mode bits only;
extended ACLs are not inspected. Do not place journals under a directory with
inheritable ACLs.

Journal and audit paths must name a file: an empty path, a trailing `/`, or a
final `.` or `..` is refused. Durations in the configuration must be whole
milliseconds, the unit the journal records.

## Checks

Host-side tests use a fake kilnd adapter:

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
```

The installed-service test is opt-in and must run inside the kiln VM:

```sh
scripts/integration-vm.sh
```

Guest prerequisites: a Rust toolchain at MSRV 1.88 or newer, `python3`, `sudo`,
the installed kilnd and kiln, and Ubuntu's `acl` package (`setfacl` and
`getfacl`, which the script requires so the Linux ACL tests run).

It requires no active sandboxes or running executions, verifies all five tool
results and execution links, replays the recorded journal against a second
sandbox, and runs the `demo`, `journal verify`, and `replay` commands.
Afterwards, node reservations and the set of non-deleted sandboxes must match
a snapshot taken before the run, since a node may keep sandboxes the test does
not own. kiln's registry, run directories, cgroups, and shim/init processes are
checked before and after. It does not run benchmark campaigns.

## Next step: hosted providers

Hosted APIs come next, as further adapters under `src/providers/` with the
same contracts: translate `ModelRequest` and `ModelResponse`, own the retry
policy (the loop treats every provider error as fatal), bound response bodies,
normalize usage, keep keys host-side, and cancel HTTP work at the session
deadline.

See `docs/architecture.md` and `docs/threat-model.md` for lifecycle and trust
details.

## License

Copyright 2026 Stephen Estrada. Licensed under the
[Apache License, Version 2.0](LICENSE).
