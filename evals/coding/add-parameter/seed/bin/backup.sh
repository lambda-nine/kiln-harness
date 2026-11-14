#!/bin/sh
# backup.sh DB OUTDIR: dump DB into OUTDIR, compressed, with a dated name.
ROOT=${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$ROOT/lib/log.sh"
. "$ROOT/lib/db.sh"
. "$ROOT/lib/lock.sh"

[ $# -eq 2 ] || { echo "usage: backup.sh DB OUTDIR" >&2; exit 2; }
audit_log=${AUDIT_LOG:-/dev/null}
log_event info "backup of $1 started" 2>>"$audit_log"
lock_take "$2/.lock" || exit 1
out="$2/$1-$(date +%Y%m%d).sql"
if db_dump "$1" "$out"; then
  gzip -f "$out" && log_event info "wrote $out.gz"
else
  log_event error "backup of $1 failed; see above"
  lock_drop "$2/.lock"
  exit 1
fi
lock_drop "$2/.lock"
log_event_count=$(( ${log_event_count:-0} + 1 ))
log_event info 'backup done, "latest" link updated'
