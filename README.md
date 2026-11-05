# kiln-harness

A provider-neutral agent loop for running tool calls in
[kiln](https://github.com/lambda-nine/kiln) sandboxes. It talks only to
[kilnd](https://github.com/lambda-nine/kilnd)'s `kilnd.v1` gRPC API over a Unix
socket: one no-network sandbox per session, five typed tools, hard budgets, a
metadata-only audit log, and verified sandbox deletion after every session.

**Status: experimental.** This milestone has only a deterministic scripted
provider, so it needs no model account, API key or network access; no live
model provider is implemented yet. kiln is experimental shared-kernel
isolation and **not a boundary for hostile multi-tenant workloads**. Report
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

Host-side tests use a fake kilnd and run on Linux or macOS with rustup
(`rust-toolchain.toml` selects Rust 1.88) and `protoc`
(`sudo apt-get install protobuf-compiler` or `brew install protobuf`):

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
(`exec`, `read_file`, `write_file`, `list_dir`, and `workspace_diff`), applies
hard step/call/input/output/deadline budgets, writes a metadata-only append-only
JSONL audit with host-generated call correlation IDs, and retries sandbox
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

The script writes and reads `/workspace/demo.txt`, lists the workspace, checks
the session file-set diff, and runs `wc` with literal argv. The scripted
provider verifies every result before emitting its final answer. Ctrl-C and
wall deadlines stop waiting on an active RPC; kilnd is asked to cancel it, then
deletion is retried and verified within a separate bounded cleanup window.

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

It requires an empty baseline, verifies all five tool results and execution
links, and checks API reservations, kiln's registry, run directories, cgroups,
and shim/init processes before and after the test. It does not run benchmark
campaigns.

## Next step: live provider

Add one provider adapter in a separate change. It should translate the existing
provider trait to a chosen model API, keep credentials in the host vault or
environment, enforce request/token/cost budgets, cancel HTTP work at the
session deadline, and retain the same tool and audit contracts. No paid
provider is selected or called in this milestone.

See `docs/architecture.md` and `docs/threat-model.md` for lifecycle and trust
details.

## License

Copyright 2026 Stephen Estrada. Licensed under the
[Apache License, Version 2.0](LICENSE).
