# kiln-harness

The smallest useful provider-neutral agent loop over `kilnd.v1`. This milestone
uses a deterministic scripted provider, so it needs no model account, API key,
or network access.

It creates one no-network sandbox per session, exposes five typed tools
(`exec`, `read_file`, `write_file`, `list_dir`, and `workspace_diff`), applies
hard step/call/input/output/deadline budgets, writes a metadata-only append-only
JSONL audit, and retries sandbox deletion after every returned work outcome.
An unconfirmed cleanup is a bounded, explicit error rather than a hidden
success. All sandbox work goes through kilnd's Unix-socket API; the harness
never invokes `kiln` or a host shell.

kiln is experimental shared-kernel isolation. It is not a boundary for hostile
multi-tenant workloads.

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
