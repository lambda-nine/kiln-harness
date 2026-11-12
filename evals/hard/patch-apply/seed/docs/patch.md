# apply.sh: applying a unified diff

`sh /workspace/apply.sh PATCH` applies the unified diff in the file PATCH
to the tree under `/workspace/tree`. PATCH is a path, relative to the
current directory or absolute; the script works from any directory. If
PATCH cannot be read, it prints nothing on standard output, leaves the
tree untouched and exits with status 2. This document is the whole
contract.

## 1. Files and lines

Every file, the patch included, is a sequence of lines separated by LF.
The last line of a file may lack its LF. A CR is an ordinary byte: a line
`a<CR><LF>` is the text `a<CR>` followed by LF, and must be kept that way.
A *line* below is its text together with whether it ends with LF, so the
line `a` with an LF and the line `a` without one are different lines.

## 2. The patch

### 2.1 File sections

A patch is a list of file sections. A file section starts with two header
lines:

```text
--- OLD
+++ NEW
```

Each header line starts with `--- ` or `+++ ` (three characters and a
space). The path is the rest of the line up to its first tab, if any;
anything after that tab, such as a timestamp, is ignored. A path is either
exactly `/dev/null` or has its first component removed: everything up to
and including its first `/` is dropped, so `a/src/main.c` names
`src/main.c` in the tree. A path other than `/dev/null` that contains no
`/` is malformed.

- If OLD is `/dev/null`, the section **creates** the file NEW.
- If NEW is `/dev/null`, the section **deletes** the file OLD.
- Otherwise the section **patches** the file NEW (OLD is not used).
- Both `/dev/null` is malformed.

The file a section names (NEW, or OLD for a deletion) is written PATH
below. A patch names each file at most once.

### 2.2 Hunks

After its header lines a file section has zero or more hunks (with none,
it changes no lines). A hunk starts with a header line:

```text
@@ -OLDSTART,OLDCOUNT +NEWSTART,NEWCOUNT @@ optional text
```

The four numbers are decimal. `,OLDCOUNT` or `,NEWCOUNT` may be left out,
and then means 1. The header ends with ` @@`; anything after it is ignored.
Then come the hunk's body lines, each starting with one character:

- ` ` (space): a context line, in both the old and the new file;
- `-`: a line only in the old file;
- `+`: a line only in the new file.

The rest of a body line (after its first character) is the line's text.
The body has exactly OLDCOUNT lines that are context or `-`, and exactly
NEWCOUNT lines that are context or `+`, in any order; it ends as soon as
both counts are reached.

A line starting with `\` (normally `\ No newline at end of file`) may
follow any body line, including the last one of a hunk. It says that the
body line before it has no LF: in the old file if it is a `-` line, in the
new file if it is a `+` line, and in both if it is a context line. It is
not itself a body line and is not counted.

`-0,0` means the old side has no lines at all, as for a new file.

### 2.3 Everything else

Outside hunk bodies, any line that does not start with `--- `, `+++ `,
`@@`, a space, `-`, `+` or `\` is ignored. That includes lines before the
first file section, such as `diff --git a/x b/x` and `index 3f2a..9c1b`,
and empty lines.

### 2.4 Malformed patches

A patch is malformed if any of these holds; N is the line number in PATCH
(from 1) where the problem is found:

- a `--- ` line that is not directly followed by a `+++ ` line (N is the
  `--- ` line);
- a `+++ ` line that does not directly follow a `--- ` line (N is that
  line);
- a malformed header path, or both paths `/dev/null`, as in 2.1 (N is
  the `+++ ` line);
- a line starting with `@@` that is not a valid hunk header, or that comes
  before any file section (N is that line);
- in a hunk body, a line that does not start with a space, `-`, `+` or
  `\`, or a `\` line that does not follow a body line (N is that line);
- a hunk body cut short by the end of the patch (N is the hunk's header
  line);
- outside hunk bodies, a line starting with a space, `-` (but not `--- `),
  `+` (but not `+++ `) or `\`: a body line beyond its hunk's counts (N is
  that line).

The whole patch is checked, from its first line to its last, before
anything else is done. For a malformed patch, print
`malformed patch at line N` on standard error for the first problem found
(the smallest N), print nothing on standard output, leave the tree
untouched and exit with status 2.

## 3. Applying

### 3.1 One file

A file's hunks are applied in order, each to the file as the earlier hunks
left it. A hunk's *old lines* are its context and `-` lines in order; its
*new lines* are its context and `+` lines in order. Applying a hunk at
position P replaces the old lines, which must be lines P, P+1, … of the
file, with the new lines.

The hunk's *expected position* is OLDSTART + SHIFT, or OLDSTART + 1 + SHIFT
when OLDCOUNT is 0 (then the new lines go after line OLDSTART). SHIFT
starts at 0 for each file. After a hunk applies at position P, SHIFT
becomes P − OLDSTART (or P − OLDSTART − 1 when OLDCOUNT is 0) plus
NEWCOUNT − OLDCOUNT. So each hunk's expected position accounts both for the
lines the earlier hunks added or removed and for how far from their own
expected positions they actually applied.

A hunk *matches* at P when its old lines are exactly lines P, P+1, … of the
file, LF included. P may run from 1 to the number of lines in the file
minus OLDCOUNT, plus 1. To find where a hunk applies, try its expected
position E first, then E+1, E−1, E+2, E−2, and so on, skipping positions
outside that range, until every position in the range has been tried. The
hunk applies at the first position that matches. If none does, the hunk
fails. There is no fuzz: every old line must match.

### 3.2 Each kind of section

- **Patch:** PATH must exist as a file. Its hunks are applied to it.
- **Create:** PATH must not exist. Its hunks are applied to an empty file
  (no lines), and the result is written to PATH. Missing parent
  directories are created.
- **Delete:** PATH must exist as a file. Its hunks are applied to it, and
  must leave it empty (no lines); then PATH is removed. Its directory is
  left in place, even if it is now empty.

### 3.3 All or nothing

The sections are processed in patch order. The run fails at the first
problem:

- `cannot patch PATH`: a file to patch or delete is missing, a file to
  create already exists, or a deletion's hunks applied but did not leave
  the file empty;
- `hunk K of PATH failed`: the Kth hunk of that file section (counting from
  1) matches nowhere.

On a failure, print that one line on standard error, print nothing on
standard output, and exit with status 1, with the tree **exactly as it
was**: no file changed, created or removed, and no directory created, even
for sections before the failing one.

On success, every section takes effect. Print one line per section, in
patch order, on standard output: `patched PATH`, `created PATH` or
`deleted PATH`, and exit with status 0. Files that are patched keep their
permissions.
