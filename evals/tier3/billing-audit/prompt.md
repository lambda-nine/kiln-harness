`/workspace/billing` is a small billing program. Given a directory of data files and a month, it prints that month's invoices.

- `bin/bill`: the command, a POSIX `sh` script that runs the awk modules in `lib/` as one program.
- `lib/*.awk`: the program itself, in POSIX `awk`.
- `docs/MANUAL.md`: the specification: the data files, how every invoice is worked out, the output format and the errors.
- `tests/run.sh`: the test suite. It passes now.
- `examples/`: sample data directories. `examples/NAME-PERIOD.txt` is exactly what `bill examples/NAME PERIOD` must print.
- `ISSUES.md`: two problems that customers have reported.

`bill` must follow `billing/docs/MANUAL.md` exactly: where the program and the manual disagree, the manual is right. Fix the two problems reported in `billing/ISSUES.md`. The program also departs from the manual in other places that nobody has reported and that the tests and examples do not show. Find and fix those too, by checking the code against each rule in the manual. Making the reported examples come out right is not enough, and the program must not change anywhere it already follows the manual.

Rules:

- Fix the program in place. Change only files under `billing/bin/` and `billing/lib/`. Do not change, add or remove anything under `billing/docs/`, `billing/tests/` or `billing/examples/`, and do not change `billing/ISSUES.md`.
- The program must stay POSIX `sh` and POSIX `awk`, run as `sh /workspace/billing/bin/bill DATADIR PERIOD` from any directory.
- When you are done, `sh /workspace/billing/tests/run.sh` must still pass.
- The checker runs `tests/run.sh` once, which must finish within 15 seconds, then runs `bill` once for each of its tests, and each of those runs must finish within 5 seconds.

Use only the standard tools available here; there is no network. Commands you run start in `/workspace`.
