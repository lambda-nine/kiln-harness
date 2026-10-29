# kiln-harness: notes for agents

This repository is the provider-neutral agent layer over `kilnd.v1`. Read
`README.md`, `docs/architecture.md`, and `docs/threat-model.md` before changing
behavior.

- Run `cargo test --locked`, `cargo clippy --locked --all-targets -- -D warnings`,
  and `git diff --check` before each commit. The MSRV is Rust 1.88.
- The harness talks only to kilnd over its Unix socket. Never invoke `kiln`,
  inspect kiln run directories, or construct a host shell command from model
  output in production code.
- Model arguments and sandbox output are untrusted. Keep provider keys and
  other credentials host-side and out of sandboxes and audit records.
- Default network policy is none. Any broader policy must be explicit and
  remain expressed in backend-neutral kilnd API terms.
- One sandbox belongs to one session. After success, cancellation, deadline,
  or error, deletion must be retried and absence verified within the lifecycle
  window; never hide an unconfirmed cleanup.
- kiln is experimental shared-kernel isolation, not a hostile multi-tenant
  security boundary.
- Commits are Stephen Estrada <stcrestrada@gmail.com> only, with no
  `Co-Authored-By` trailers.
