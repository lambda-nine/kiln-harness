#!/bin/sh
# compare.sh NEW OLD: how much two scans differ in total, and the largest file
# of each.
DUS_ROOT=${DUS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
for lib in util size scan report/diff; do . "$DUS_ROOT/lib/$lib.sh"; done

[ $# -eq 2 ] || die "usage: compare.sh NEW OLD"
echo "change: $(scan_diff "$1" "$2")"
for f in "$1" "$2"; do
  set -- $(scan_largest "$f")
  echo "largest in $f: $1 (`fmt_size $2`)"
done
