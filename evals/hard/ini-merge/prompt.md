Write a POSIX shell script `/workspace/merge.sh` that merges layered INI files. It will be run as `sh /workspace/merge.sh FILE...` with one or more files, from any directory. Later files override earlier ones. Use only POSIX `sh` and the standard tools available here (`awk`, `sed`, `sort`, `grep` and so on); there is no network.

## Line syntax

Files are LF-terminated text (the last line may have no LF). "Blanks" below means spaces and tabs. Each line is exactly one of:

- **Blank line:** empty or only blanks. Ignored.
- **Comment:** optional blanks, then `;` or `#`, then anything. Ignored.
- **Section header:** optional blanks, `[`, optional blanks, a name, optional blanks, `]`, optional blanks. For example `[server]` or `  [ Server ]`.
- **Key line:** optional blanks, a key, optional blanks, `=`, then the value. The value is everything after the first `=`, with leading and trailing blanks removed. It may contain `=` (and `;` or `#`, which are not comments there), and it may be empty.

Section names and keys are one or more characters from `A-Z`, `a-z`, `0-9`, `.`, `_` and `-`. Any other line is malformed.

## Merging

- In each file, key lines before that file's first section header belong to the global section.
- Section names and keys are case-insensitive: `[Server]` and `[server]` are the same section, `Port` and `port` the same key. Print them in lower case.
- A later value for the same key replaces the earlier one. A value of exactly `!unset` (after trimming) removes the key from the merged result instead; a later file may set it again.
- A section with no keys left is not printed.

## Output

1. First the global keys, without a header; then each section, as a line `[name]` followed by its keys.
2. Order sections by their first appearance (counting header lines) across all files in argument order, and each section's keys by their first appearance (counting every key line, including `!unset` ones). A key that is removed and set again keeps its first position.
3. Print each key as `key=value`, with nothing around the `=`.
4. Separate printed groups with one empty line. No empty line at the start or end. If nothing is left, print nothing.

## Errors

Read the files in argument order and stop at the first problem:

- A file that does not exist or cannot be read: print nothing on stdout and exit 2.
- A malformed line: print nothing on stdout, print exactly one line `FILE:LINE: malformed` on stderr (`FILE` exactly as given on the command line, `LINE` counting from 1), and exit 3.

Otherwise exit 0. For example, `cd /workspace && sh merge.sh base.ini prod.ini` must print:

```text
name=shop
debug=false

[server]
host=0.0.0.0
port=80
workers=4
tls=on

[database]
url=postgres://db.internal/shop
pool=20
password=

[metrics]
endpoint=/metrics
```

Leave `base.ini` and `prod.ini` unchanged. Commands you run start in `/workspace`.
