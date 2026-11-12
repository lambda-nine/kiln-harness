# strutil

`lib/strutil.sh` is a library of string functions for POSIX `sh`. Source it
with `. lib/strutil.sh`. Every function takes its arguments as separate
words, never reads standard input, and changes no variables other than its
own `_strutil_*` working variables.

Strings are byte strings without NUL bytes or newlines. Lengths and
positions are in bytes. "Prints" means writes to standard output; a function
that returns 2 prints nothing at all. Unless stated otherwise a function
returns 0.

## `str_trim S`

Prints `S` with every leading and trailing space and tab removed, followed by
a newline. Spaces and tabs between other characters are kept. Other
characters, such as carriage returns, are never removed. A string of only
spaces and tabs prints an empty line.

## `str_pad_left S WIDTH [CHAR]`

Prints `S` with copies of `CHAR` added on the left until it is `WIDTH` bytes
long, followed by a newline. `CHAR` defaults to a space. If `S` is already
`WIDTH` bytes or longer it is printed unchanged; it is never truncated.

- `WIDTH` must be one or more decimal digits (`0` is allowed); otherwise,
  including for a negative number or an empty string, return 2.
- If `CHAR` is given it must be exactly one byte; otherwise, including for
  an empty string, return 2.

## `str_repeat S N`

Prints `S` repeated `N` times, then a newline. `N` = 0 prints just a newline,
and an empty `S` prints just a newline for any valid `N`. `N` must be one or
more decimal digits; otherwise, including for a negative number, return 2.

## `str_count S SUB`

Prints the number of non-overlapping occurrences of `SUB` in `S`, counted
from left to right, followed by a newline: after each match the search
continues after the end of that match. `str_count aaaa aa` prints `2`. `SUB`
is literal text: characters such as `*`, `?`, `[` and `\` match only
themselves. An empty `SUB` returns 2.

## `str_lower S`

Prints `S` with the ASCII letters `A` to `Z` changed to `a` to `z`, followed
by a newline. Every other byte, including bytes above 127, is unchanged.

## `str_starts_with S PREFIX`

Returns 0 if `S` begins with `PREFIX`, and 1 otherwise. It prints nothing.
`PREFIX` is literal text: characters such as `*`, `?` and `[` match only
themselves. Every string begins with the empty prefix.
