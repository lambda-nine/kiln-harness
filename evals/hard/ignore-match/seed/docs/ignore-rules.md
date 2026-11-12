# Ignore rules

An ignore-rules file lists patterns that decide which paths in a tree are
ignored. The rules are a subset of a familiar format, but this document is
the whole contract: where it differs from any tool you know, follow this
document.

## 1. Paths

A path is relative to the root of the tree and uses `/` between its
segments, for example `src/lib/util.c`. It never starts with `/`, never
contains an empty segment, and has no `.` or `..` segments. A path that
ends with `/` names a directory, for example `src/lib/`; any other path
names a file.

Below, a path's *segments* are the parts between the `/`s, ignoring the
final `/` of a directory path, and its *name* is its last segment. So
`src/lib/` has the segments `src` and `lib` and the name `lib`.

## 2. Reading the rules file

The rules file is LF-separated lines, numbered from 1. Each line is read
on its own, in these steps:

1. **Escapes.** A backslash makes the character after it literal: it loses
   any special meaning described below and is matched as itself. `\\` is a
   literal backslash, `\*` a literal `*`, `\ ` a literal space. `\/` counts
   as a plain `/` everywhere below. A backslash at the very end of the line
   (after step 2) has nothing to escape, and the line is invalid.
2. **Trailing spaces.** Spaces at the end of the line are removed, one by
   one, up to the first one that is escaped. So `a  ` becomes `a`, and
   `a\ ` and `a\  ` both become `a\ `, the name `a` followed by one space.
   Only spaces are removed this way; tabs are kept.
3. **Blank lines and comments.** A line that is now empty is skipped. A
   line whose first character is an unescaped `#` is a comment and is
   skipped, whatever else it contains. A line starting with `\#` is a pattern starting with a literal
   `#`.
4. **Negation.** If the first character is an unescaped `!`, the rule is a
   *negation* and the `!` is removed. A line starting with `\!` is a
   pattern starting with a literal `!`.
5. **Directory-only.** If the pattern now ends with `/`, the rule is
   *directory-only* and that one `/` is removed.
6. **Anchoring.** If the pattern now contains a `/` anywhere, the rule is
   *anchored*, and a leading `/`, if there is one, is removed. Otherwise
   the rule is *unanchored*.
7. **Nothing left.** If the pattern is now empty (for example the line is
   `!`, `/` or `!/`), the line is invalid.

The pattern left after these steps is split into *pattern segments* at
each `/` (anchored rules) or is one pattern segment (unanchored rules).

## 3. Matching one pattern segment

A pattern segment is matched against one path segment, from start to end,
character by character. Comparison is by bytes and case-sensitive. There
is no special case for names that start with `.`.

- `*` matches any run of characters, including none.
- `?` matches exactly one character.
- `[...]` is a *class* and matches exactly one character:
  - A `!` right after the `[` makes the class negated: it matches one
    character that is *not* a member.
  - The class ends at the first `]` that is not its first member: a `]`
    right after `[` or `[!` is a member. If the end of the pattern segment
    comes first, the class is unterminated and the line is invalid.
  - A member is one character, or a range `x-y`, which covers every byte
    from `x` to `y` inclusive (none if `x` is greater than `y`). A `-`
    that is the first or last member is literal.
  - Inside a class a backslash makes the next character literal, so
    `[\]]` matches `]`.
- `**` inside a pattern segment that has other characters, as in `a**b` or
  `**.txt`, matches like `*`. A pattern segment that is exactly `**` also
  matches like `*` when it is the rule's only pattern segment (the lines
  `**` and `/**`); as one of several, it is described in section 4.
- Every other character, and every escaped character, matches itself.

Because a pattern segment is matched against one path segment, `*`, `?`
and classes can never match a `/`.

## 4. Matching a rule against a path

A rule matches a path when all of these hold:

- If the rule is directory-only, the path names a directory.
- If the rule is unanchored, its pattern segment matches the path's name.
  So `foo` matches `foo`, `a/foo` and `a/b/foo/`, but not `food` or
  `foo/bar` (though see section 5 for `foo/bar`).
- If the rule is anchored, its pattern segments match the path's segments
  one for one, from the first: `doc/*.txt` matches `doc/a.txt` but not
  `doc/sub/a.txt` or `x/doc/a.txt`. A pattern segment that is exactly `**`
  instead matches a run of whole path segments:
  - as the first of several pattern segments (`**/build`), any number of
    segments, including none: `build`, `a/build` and `a/b/build`;
  - between two pattern segments (`a/**/b`), any number of segments,
    including none: `a/b`, `a/x/b` and `a/x/y/b`;
  - as the last of several pattern segments (`logs/**`), one or more
    segments: `logs/a` and `logs/a/b/`, but not `logs` or `logs/`
    themselves.

The rule `/foo` is anchored, so it matches `foo` and `foo/` but not
`a/foo`. `a/foo` is anchored too, so it matches `a/foo` but not `b/a/foo`.

## 5. Which paths are ignored

Rules are considered in file order, and **the last rule that matches a
path decides** for it: the path is *excluded* if that rule is not a
negation. If no rule matches, it is not excluded.

A path is **ignored** if it is excluded, or if any of its ancestor
directories is excluded. The ancestors of `a/b/c.txt` are the directory
paths `a/` and `a/b/`, and each is judged as a directory path by the same
last-match rule. So once a directory is excluded, everything under it is
ignored, and no negation can bring back a path below it.

For example, with these rules:

```text
build/
!build/keep.txt
*.log
!important.log
out/*
!out/keep/
```

- `build/keep.txt` is ignored: the negation matches it, but its ancestor
  `build/` is excluded.
- `x/important.log` is not ignored: `*.log` matches it, but the later
  `!important.log` also matches, and the last match decides.
- `out/keep/a.txt` is not ignored: no rule matches it, and of its
  ancestors, `out/` matches nothing and `out/keep/` is matched last by the
  negation.
- `out/keep` (a file) is ignored: `!out/keep/` is directory-only, so the
  last rule matching it is `out/*`.
- `build` (a file) is not ignored, since `build/` is directory-only.

## 6. The program

`sh ignored.sh RULES` reads the rules file RULES, then reads paths from
standard input, one per line, and prints the ignored ones on standard
output, one per line, exactly as they were read and in input order (a
path given twice is printed twice). It prints nothing else and exits with
status 0.

Errors, checked before any path is read or printed:

- If RULES cannot be read, print nothing on standard output and exit with
  status 2.
- If a line of RULES is invalid (an unterminated class, a backslash at the
  end of the line, or nothing left of the pattern), print nothing on
  standard output, print exactly one line `RULES:LINE: bad pattern` on
  standard error, with RULES exactly as given on the command line and LINE
  the number of the first invalid line, and exit with status 3.
