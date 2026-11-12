Write a POSIX shell script `/workspace/order.sh` that prints a build order for a dependency file. It will be run as `sh /workspace/order.sh FILE`, from any directory. Use only POSIX `sh` and the standard tools available here (`awk`, `sed`, `sort`, `grep` and so on); there is no network.

## Input

`FILE` is a text file of LF-terminated lines (the last may have no LF). Line numbers count every line from 1.

- A line that is empty or holds only spaces and tabs is ignored.
- A line whose first character other than a space or tab is `#` is a comment and is ignored.
- Every other line must have this form: optional spaces or tabs, a target name, optional spaces or tabs, `:`, then zero or more dependency names separated by runs of spaces or tabs, and optional trailing spaces or tabs. For example `app: lib util`, `app:lib`, or `  clean:`.
- A name is one or more characters from `A-Z`, `a-z`, `0-9`, `.`, `_` and `-`. Nothing else may appear on a line, so `#` after a name is an error, not a comment.
- A target may appear on several lines; its dependencies are the union of all of them. Repeating a dependency changes nothing.

## Output

Print every name that appears anywhere in the file (as a target or as a dependency) exactly once, one per line, each followed by LF, so that every name comes after all of its dependencies.

Whenever more than one name is ready (all its dependencies already printed), print the smallest one first, comparing names byte by byte in ASCII order (so `B` comes before `a`, and `10` before `9`). That makes the answer unique.

## Errors

Check the syntax of the whole file first.

- If any line breaks the syntax, print nothing on stdout, print exactly one line `syntax error on line N` on stderr for the first such line, and exit 2.
- Otherwise, if the dependencies contain a cycle (including a name that depends on itself), print nothing on stdout, print exactly one line `cycle` on stderr, and exit 3.
- Otherwise exit 0. A file with no names at all prints nothing and exits 0.

For example, `sh /workspace/order.sh /workspace/deps.txt` must print:

```text
config.h
fixtures
icons
libc
lib-gfx
lib-net
lib-ui
app
test
```

Leave `deps.txt` unchanged. Commands you run start in `/`, so use absolute paths or `cd /workspace` first.
