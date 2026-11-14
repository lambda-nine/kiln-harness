#!/bin/sh
# topn.sh N FILE: the sizes of the N largest files, largest first, one per
# line.
DUS_ROOT=${DUS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
export DUS_ROOT
grep -v '^#' "$2" | awk '{ print $2 }' | sort -nr | head -n "$1" |
  xargs -n 1 sh -c '. "$DUS_ROOT/lib/size.sh"; fmt_size "$1"' sh
