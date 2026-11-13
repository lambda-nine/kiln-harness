Write a POSIX shell script `/workspace/ignored.sh`. It will be run as `sh /workspace/ignored.sh RULES`, from any directory, with a list of paths on standard input, one per line. It prints the paths that the ignore-rules file RULES ignores, in input order.

The rules format and the program's exact behaviour, including its errors and exit statuses, are specified in `/workspace/docs/ignore-rules.md`. That document is the contract. It looks like a format you may know, but it is a stated subset with its own rules; where it differs from any tool you know, follow the document. Read all of it, including the worked example in section 5. The check runs many rules files and paths that are not in the workspace, covering every rule in the document.

For example, `sh /workspace/ignored.sh /workspace/examples/rules < /workspace/examples/paths` must print exactly:

```text
src/main.o
src/util.c~
target/
target/debug/app
.DS_Store
web/.DS_Store
logs/today.txt
app.log
docs/api/search.html
```

Rules:

- Do not change, add or remove anything under `docs/` or `examples/`.
- Use only POSIX `sh` and the standard tools available here (`awk`, `sed`, `grep` and so on). Do not use `git` or any other program that implements ignore rules for you; there is no network.

Commands you run start in `/workspace`.
