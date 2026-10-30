# Threat boundaries

The model, its arguments, tool results, sandbox output, and files are untrusted.
The host operator, kilnd socket owner/group, harness binary, and configured
provider credential source are trusted. Anyone who can reach the kilnd socket
has near-root sandbox-control capability on this development node.

kiln supplies namespace, cgroup, quota, seccomp, Landlock, and network
isolation. It shares the host kernel, is experimental, and has not been audited;
it is not a hostile multi-tenant boundary.

## Enforced here

- Only `kilnd.v1` is used. Production code never starts `kiln`, reads its
  filesystem state, or runs a host shell.
- Every session requests network none. This milestone has no live provider and
  handles no API key.
- Typed JSON arguments reject unknown fields. File helpers accept only strict
  relative `/workspace` paths; `exec` remains a deliberately general sandbox
  argv primitive.
- Step, call, argv, stdin, file, manifest, result, per-execution, and session
  bounds are finite. kilnd supplies a second validation and capture boundary.
- Provider, audit, budget, cancellation, deadline, and tool errors all enter a
  separate bounded cleanup path. Every delete acknowledgement and ambiguous
  error is independently checked with a complete non-deleted listing. Delete
  and verification are retried within the same cleanup window; failed or
  stalled verification is surfaced as unconfirmed cleanup.
- Create recovery classifies typed gRPC status codes: invalid argument, resource
  exhausted, failed precondition, permission denied, and unauthenticated are
  definite rejections only after successful recovery listings. Other codes and
  local transport/protocol failures remain ambiguous without a recovered row.
- Audit records omit content. They store hashes, byte/truncation evidence,
  host-generated call correlation IDs, sandbox IDs, and kilnd execution IDs. Raw
  provider call IDs stay only in the transient provider transcript.

## Deliberate limits

- Lexical workspace validation is not a filesystem capability system. A
  symlink under `/workspace` can target another sandbox path; read-only root
  blocks helper writes outside approved writable paths, and the sandbox—not the
  workspace prefix—is the host boundary. General `exec` can read any path the
  sandbox policy permits.
- Cancellation races active provider and tool RPCs. Dropping an Exec RPC asks
  kilnd to cancel its daemon-owned execution; sandbox deletion then provides
  the cleanup barrier. Create response loss is recovered by unique session
  labels. Lifecycle recovery may outlive the work deadline; its default is 10
  seconds and configurations above 60 seconds are rejected. Exhaustion is an
  explicit `Cleanup` or `CreateUncertain` error.
- SIGKILL, host loss, or a harness crash can prevent client-side delete. kilnd
  reconciliation handles known state; a future sandbox TTL is still needed for
  unattended client crashes.
- The JSONL file is append-only by open mode, not tamper-proof. A trusted host
  user can modify it. Durable signing or a remote append service is out of
  scope.
- kilnd persists bounded command output in its root-only SQLite database. The
  harness audit excludes it, but callers must still avoid printing host secrets
  into a sandbox command.
