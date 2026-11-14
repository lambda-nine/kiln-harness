#!/bin/sh
# rotate.sh FILE KEEP: rotate FILE to FILE.1 ... FILE.KEEP.
ROOT=${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$ROOT/lib/log.sh"

file=$1
keep=${2:-5}
[ -f "$file" ] || { log_event warn "nothing to rotate: $file"; exit 0; }
i=$keep
while [ "$i" -gt 1 ]; do
  prev=$(( i - 1 ))
  if [ -f "$file.$prev" ]; then mv "$file.$prev" "$file.$i" || log_event error "could not move $file.$prev"; fi
  i=$prev
done
mv "$file" "$file.1" && : >"$file" || log_event error "could not rotate $file"
log_event info done
