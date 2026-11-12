`/workspace/ledger` is a small plain-text accounting program:

- `bin/ledger`: the command, a POSIX `sh` wrapper that runs `lib/ledger.awk`.
- `lib/ledger.awk`: the program itself, in POSIX `awk`.
- `docs/MANUAL.md`: the specification of the journal format, every command, the output formats and the errors.
- `tests/run.sh`: the test suite. It passes now.
- `examples/`: sample journals.
- `ISSUES.md`: problems that users have reported.

Users reported the three problems in `ledger/ISSUES.md`. Fix them. Then make `ledger` conform to `ledger/docs/MANUAL.md` in every respect: the manual is the authority, and other defects may exist that nobody has reported and the tests do not cover. Read the manual and the code carefully and check each rule. Making the reported examples work is not enough.

Rules:

- Change only `lib/ledger.awk`, and `bin/ledger` if you need to. Do not change, add or remove anything under `docs/`, `tests/` or `examples/`, and do not change `ISSUES.md`.
- The program must stay POSIX `sh` and POSIX `awk`, run as `sh /workspace/ledger/bin/ledger COMMAND FILE [PREFIX]` from any directory.
- When you are done, `sh /workspace/ledger/tests/run.sh` must still pass.

Use only the standard tools available here; there is no network. Commands you run start in `/`, so use absolute paths.
