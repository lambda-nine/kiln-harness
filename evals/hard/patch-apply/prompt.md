Write a POSIX shell script `/workspace/apply.sh` that applies a unified diff to the tree under `/workspace/tree`. It will be run as `sh /workspace/apply.sh PATCH`, from any directory.

The contract is in `/workspace/docs/patch.md`: the patch format it accepts, how hunks are placed (including the offset search and how each hunk's expected position depends on the hunks before it), files without a final newline, creating and deleting files, malformed patches, and the exact output, error messages and exit statuses. Read all of it. The patches you may have seen other tools accept are not the contract; the document is. In particular, a run either applies the whole patch or leaves the tree exactly as it was.

`/workspace/example.patch` is an example. Applied to the tree as it is now, it must print exactly

```text
patched src/config.h
patched src/server.c
created docs/USAGE.md
```

and exit with status 0, having changed the default port in `src/config.h` to 8080, made the four changes its hunks describe in `src/server.c`, and created `docs/USAGE.md` (and the `docs/` directory) with the five lines of its last hunk. Nothing else in the tree changes. Its third `src/server.c` hunk is not where its header says, so it shows the offset search at work; work out where each hunk applies before you rely on it.

The check runs many patches that are not in the workspace, each against a fresh copy of the tree, so you may change `/workspace/tree` while you test. Do not change `docs/` or `example.patch`.

Use only POSIX `sh` and the standard tools available here (`awk`, `sed`, `cmp`, `mktemp` and so on). Do not use `patch`, `git` or any other program that applies diffs for you; there is no network. Commands you run start in `/workspace`.
