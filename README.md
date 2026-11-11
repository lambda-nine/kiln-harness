# kiln-harness

A provider-neutral agent loop for running tool calls in
[kiln](https://github.com/lambda-nine/kiln) sandboxes. It talks only to
[kilnd](https://github.com/lambda-nine/kilnd)'s `kilnd.v1` gRPC API over a Unix
socket: one no-network sandbox per session, five typed tools, multi-call model
turns, hard budgets, a metadata-only audit log, a content-bearing session
journal that replays deterministically, and verified sandbox deletion after
every session.

**Status: experimental.** Besides a deterministic scripted provider, which
needs no model account, API key or network access, it has two live
providers: an adapter for the OpenAI-compatible `/v1/chat/completions` API
that the MLX server, Ollama, llama.cpp, and vLLM speak, for running local
models, and an adapter for Anthropic's Messages API. kiln is
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

- kiln, tested: `00d61c546db7aa67ad283d10c2ca6165ca5f8166`. The integration
  run requires exactly this kiln.
- kiln, minimum: `e48f0e785ad484d0b2947897f6d48d0756059d7e`, the oldest kiln
  with every feature this kilnd requires (persistent-exec v1 and
  persistent-create certainty). For reference only; nothing enforces it.
- kilnd API and runtime semantics: `853dee2a827b7f60af16f731a54309b1be97f680`
  (parent `6543812a2db4af4493ad6c5ea8864c27d173195d`)

The compatible `kilnd.v1` protobuf schema is vendored under `proto/` so this
repository builds against the reviewed API even if a sibling checkout is on a
different branch. `scripts/integration-vm.sh` refuses to run against an
installed kiln other than the tested one.

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

## Sandbox expiry

Each session first reads kilnd's Health and uses only the features a
serving daemon lists. When it lists sandbox expiry, the create request
carries an expiry policy, so a harness that crashes or is killed cannot
hold its sandbox forever: kilnd reclaims it once it has been idle for
`--sandbox-idle-timeout-secs` (120 by default, 10 to 3,600), or at its
maximum lifetime, whichever comes first. The lifetime is the session
timeout plus the 10-second cleanup window plus
`--sandbox-lifetime-margin-secs` (60 by default, 1 to 600), clamped into
the daemon's advertised bounds; a daemon cap shorter than the session and
its cleanup window is refused before create, since the sandbox would expire
mid-session. Executions count as activity on kilnd's side, so tool calls
need nothing more. A model call can take minutes with no execution, so
while one runs the harness renews the sandbox a third of the idle timeout
(at least a second) after the last activity it can prove kilnd saw, on
one schedule for the whole session, so several short model calls cannot
add up to the idle timeout either. A renew refused because the sandbox
expired or was deleted, or enough failed renews in a row that it has
probably expired, ends the session with `SandboxExpired`; cleanup still
deletes and verifies absence. Heartbeats write no audit or journal records;
`session_finished` counts them as `renews`. When kilnd lists typed reasons,
an expiry refusal from any sandbox call ends the session the same way, and
so does an execution that reports the sandbox no longer reusable (as an
error when kilnd gives no expiry or deletion as the cause). Expiry limits
kilnd advertises inconsistently are refused before create. After a session
the CLI checks kilnd's Health within the cleanup window and only warns on
stderr if it does not answer or is not serving.

Against a daemon older than kilnd's API discovery, which lists no
features, the harness sends no expiry and records `null` for it in the
audit's `sandbox_created`. That is the degraded mode: the sandbox never
expires, and a crashed harness can leak it, as before.

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
the hard ceilings in `docs/architecture.md` still apply. The sandbox expiry
flags are described above. Reasoning tokens
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

## Running against the Anthropic API

`--provider anthropic` talks to Anthropic's Messages API directly over
HTTPS. Running it costs money on the account that owns the key. Set the key
in an environment variable yourself (the name is up to you) and name the
variable:

```sh
export ANTHROPIC_API_KEY=...   # set by the operator, never a flag
cargo run --locked -- run --provider anthropic --api-key-env ANTHROPIC_API_KEY \
  --prompt "Write a POSIX sh script in /workspace/fib.sh that prints the first ten Fibonacci numbers, run it, and report the output."
```

The model defaults to `claude-opus-5-5` (`--model` picks another) at
`--effort high` (`low`, `medium`, `high`, `xhigh`, or `max`). Requests are
not streamed. Automatic prompt caching is on, and since each request
extends the previous one unchanged, later turns read most of their prompt
from the cache; cache reads and writes are reported in usage and count
against the input budget. Server-side fallbacks are off by default.
Anthropic's guidance recommends them for production apps, where an answer
from another model beats a refusal; this harness records, replays, and
compares specific models, so it opts out for reproducibility: a refusal is
reported as a refusal by the model you asked for, not silently answered by
a different one. `--fallbacks` turns them on: if a safety check declines a
request, the API reruns it on a suitable fallback model in the same call,
and the audit records which model served each turn as `served_model`.
Every attempt then counts against the token budgets, including one that
was declined, so `--max-output-tokens` limits one model call with all its
attempts. `--strict-tools` marks the tool schemas `strict` and
sends them without the bounds strict mode refuses (`maxItems`, `minimum`,
and `minItems` above 1); the harness still enforces those bounds itself. A
schema construct strict mode does not support and the harness would not
enforce, such as `pattern` or `multipleOf`, is refused before the session
starts rather than dropped. It is off by default. An API key must be at
least 20 bytes. `--temperature` and `--seed` are
refused for this provider, and `--effort`, `--strict-tools`, and
`--fallbacks` for openai-compat. `--base-url` defaults to
`https://api.anthropic.com` and must be `https` unless its host is a
loopback address. The budget flags and defaults are the same as above.

Claude's replies come back in blocks, some of them opaque (thinking blocks
carry a signature, and a fallback leaves a marker). The harness stores each
reply's blocks exactly as received and sends them back unchanged on the
next request, as the API requires; they count toward
`--max-total-output-bytes` and are scanned for the key like everything
else. The key is sent only as the `x-api-key` header, error bodies are never
quoted, and a response containing the key is refused whole. `journal
verify` and `replay` work on these journals without the network.

### Credential proxy

The key does not have to reach the harness at all, and when the harness
runs inside a VM it should not. Run a credential-injecting proxy on the host
that forwards to `https://api.anthropic.com`, adds the `x-api-key` header,
and strips any credential headers the client sends; then point the harness
at it and leave out `--api-key-env`:

```sh
cargo run --locked -- run --provider anthropic \
  --base-url http://host.lima.internal:8788 --prompt-file task.txt
```

Without a key the harness sends no `x-api-key` or `Authorization` header and
never sees the key, so the proxy owns the job of keeping it out of what the
harness receives. Error bodies the proxy relays are quoted as for a local
model server: at most 512 bytes, without control characters. Plain `http`
is accepted to any host here, since no secret crosses that hop, but prompts
and tool results do: keep the proxy on a loopback or host-only route.
Without `--api-key-env`, a missing or default `--base-url` is refused, since
the API itself would only answer 401.

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
the installed kilnd and kiln at the commits in
[Pinned control plane](#pinned-control-plane), and Ubuntu's `acl` package (`setfacl` and
`getfacl`, which the script requires so the Linux ACL tests run).

It requires no active sandboxes or running executions, verifies all five tool
results and execution links, replays the recorded journal against a second
sandbox, and runs the `demo`, `journal verify`, and `replay` commands.
Afterwards, node reservations and the set of non-deleted sandboxes must match
a snapshot taken before the run, since a node may keep sandboxes the test does
not own. kiln's registry, run directories, cgroups, and shim/init processes are
checked before and after. It does not run benchmark campaigns.

## Adding providers

Further adapters go under `src/providers/` with the same contracts:
translate `ModelRequest` and `ModelResponse`, own the retry policy (the loop
treats every provider error as fatal), bound response bodies, normalize
usage, keep keys host-side, and cancel HTTP work at the session deadline.
The shared HTTP layer in `src/providers/http.rs` provides most of that.

See `docs/architecture.md` and `docs/threat-model.md` for lifecycle and trust
details.

## License

Copyright 2026 Stephen Estrada. Licensed under the
[Apache License, Version 2.0](LICENSE).
