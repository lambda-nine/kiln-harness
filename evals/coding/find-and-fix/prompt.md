`/workspace` holds textkit, a small library of POSIX `sh` helpers: modules
under `lib/` (some in subdirectories), commands under `bin/`, and a test
script, `tests/run.sh`.

`sh tests/run.sh`, run from `/workspace`, fails: the project gives the wrong
number of days in February for some years. Find the function that has the
bug and fix it there, so that the tests pass and everything that depends on
it gives correct answers for every year.

Do not change anything under `tests/`. Keep every other function's
behaviour, and leave no backup or temporary files behind. Commands you run
start in `/workspace`; the sandbox has busybox `sh`, `awk`, `sed` and the
other standard tools, and no network.
