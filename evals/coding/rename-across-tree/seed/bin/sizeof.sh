#!/bin/sh
# sizeof.sh: print byte counts as sizes.
DUS_ROOT=${DUS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$DUS_ROOT/lib/size.sh"

usage() {
  cat <<EOF
usage: sizeof.sh [-s] BYTES...
Prints each BYTES as fmt_size does (powers of 1024), or with -s as
fmt_size_si does (powers of 1000).
EOF
}

[ $# -gt 0 ] || { usage >&2; exit 2; }
si=0
if [ "$1" = -s ]; then si=1; shift; fi
for b in "$@"; do
  if [ $si = 1 ]; then fmt_size_si "$b"; else fmt_size "$b"; fi || exit 1
done
