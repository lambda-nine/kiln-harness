#!/bin/sh
# explain.sh BYTES...: show how each count is formatted, for bug reports.
DUS_ROOT=${DUS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$DUS_ROOT/lib/size.sh"
for b in "$@"; do
  printf 'fmt_size(%s) = %s\n' "$b" "$(fmt_size "$b")"
  printf 'fmt_size_si(%s) = %s\n' "$b" "$(fmt_size_si "$b")"
done
