# Security policy

## Supported versions

kiln-harness has no releases yet. Security fixes go to the `main` branch only.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub:
<https://github.com/lambda-nine/kiln-harness/security/advisories/new>

Please don't open a public issue, pull request or discussion for a
vulnerability. Include what you can of:

- the kiln-harness commit, and the kilnd and kiln commits it ran against
- the command line and the provider or script used
- what model output or sandbox output could make the harness do that it
  shouldn't, and the steps to reproduce it

kiln-harness is maintained by one person. Responses are best effort, with no
guaranteed response or fix time. When a fix is ready, it is disclosed through
the same advisory, with credit if you want it.

## Scope

In scope:

- the agent loop and its budgets (steps, calls, input, output, deadlines)
- the five typed tools and how their arguments reach kilnd
- the audit log: anything that writes credentials, file contents or provider
  call IDs into it, or lets untrusted input corrupt it
- sandbox lifecycle: a session that ends without its sandbox being deleted
  and its absence confirmed, without reporting an error
- anything that makes the harness invoke `kiln` or a host shell, or reach the
  host outside kilnd's Unix-socket API

Out of scope here (report them to the repository that owns them):

- escapes from a kiln sandbox: [kiln](https://github.com/lambda-nine/kiln)
- kilnd's API, state and helper: [kilnd](https://github.com/lambda-nine/kilnd)

## Security boundary today

kiln-harness is **experimental software**. It runs model-chosen tool calls in
kiln sandboxes through kilnd, and kiln is shared-kernel isolation that has been
reviewed adversarially by AI models but not audited. **It is not a boundary
for hostile multi-tenant workloads.** Don't use it to separate mutually
untrusted users.

The current milestone has only a deterministic scripted provider; no live
model provider is implemented. See [docs/threat-model.md](docs/threat-model.md).
