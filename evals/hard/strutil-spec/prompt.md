`/workspace` holds a small string library for POSIX `sh`:

- `lib/strutil.sh`: the library, six functions.
- `docs/strutil.md`: the specification of every function: its arguments, what it prints, its return codes and its edge cases.
- `tests/test_strutil.sh`: a test script. Run it from `/workspace` with `sh tests/test_strutil.sh`; it prints `ok` or `FAIL` for each test and exits non-zero if any test fails.

Make `lib/strutil.sh` conform to `docs/strutil.md` exactly, for every function and every rule in it. The specification is the authority. The tests are incomplete: some of them fail now, but making them pass is not enough, since the library has bugs the tests do not cover. Read the specification carefully and check each function against each rule.

Rules:

- Change only `lib/strutil.sh`. Do not change, add or remove anything under `docs/` or `tests/`.
- Keep the six function names and their arguments. The library must stay POSIX `sh` that works when sourced with `. lib/strutil.sh`.
- When you are done, `sh tests/test_strutil.sh` run from `/workspace` must pass.

Use only POSIX `sh` and the standard tools available here (`awk`, `sed`, `tr`, `grep` and so on); there is no network. Commands you run start in `/workspace`, so `sh tests/test_strutil.sh` runs the tests.
