Write an interpreter for Stak, a small stack language, as a POSIX shell script `/workspace/stak.sh`. It will be run as `sh /workspace/stak.sh FILE`, from any directory, where FILE is a Stak program.

The language is specified in full in `/workspace/docs/stak.md`. That document is the contract. It covers:

- tokens, comments and strings;
- every built-in word, including division and `mod` on negative numbers;
- control flow, definitions, and exactly how each word name is bound;
- syntax errors, which are found before anything runs, and runtime errors, each with its line number;
- output, standard error and exit statuses.

Read all of it before you start. The rules interact, and the check runs many programs that are not in the workspace, covering every rule in the document.

`/workspace/examples/` holds some example programs: each `NAME.stk` comes with `NAME.out`, its exact standard output. They all exit with status 0. They do not cover the error rules or every subtle case, so test those yourself against the document.

Rules:

- Do not change, add or remove anything under `docs/` or `examples/`.
- Use only POSIX `sh` and the standard tools available here (`awk`, `sed`, `grep`, `tr` and so on). There is no network.
- Speed matters: section 9 of the document sets a time limit for long loops.

Commands you run start in `/`, so use absolute paths, for example `sh /workspace/stak.sh /workspace/examples/loops.stk`.
