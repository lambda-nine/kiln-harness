cd /workspace || exit 1
cat >bin/backup.sh <<'END_FILE'
#!/bin/sh
# backup.sh DB OUTDIR: dump DB into OUTDIR, compressed, with a dated name.
ROOT=${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$ROOT/lib/log.sh"
. "$ROOT/lib/db.sh"
. "$ROOT/lib/lock.sh"

[ $# -eq 2 ] || { echo "usage: backup.sh DB OUTDIR" >&2; exit 2; }
audit_log=${AUDIT_LOG:-/dev/null}
log_event info "backup of $1 started" backup 2>>"$audit_log"
lock_take "$2/.lock" || exit 1
out="$2/$1-$(date +%Y%m%d).sql"
if db_dump "$1" "$out"; then
  gzip -f "$out" && log_event info "wrote $out.gz" backup
else
  log_event error "backup of $1 failed; see above" backup
  lock_drop "$2/.lock"
  exit 1
fi
lock_drop "$2/.lock"
log_event_count=$(( ${log_event_count:-0} + 1 ))
log_event info 'backup done, "latest" link updated' backup
END_FILE
cat >bin/nightly <<'END_FILE'
#!/bin/sh
# nightly: run every job listed in etc/nightly.conf, in order.
ROOT=${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$ROOT/lib/log.sh"
. "$ROOT/lib/fs/prune.sh"
. "$ROOT/etc/nightly.conf"

usage() {
  cat <<EOF
usage: nightly [-n]
Runs the jobs in etc/nightly.conf. With -n, only says what it would run.
Jobs log with lines such as: log_event info "backup started"
EOF
}

# step LEVEL TEXT: log a line about the run itself.
step() {
  log_event "$1" "$2" nightly
}

dry=0
case $1 in
  -n) dry=1 ;;
  '') ;;
  *) usage >&2; exit 2 ;;
esac
for job in $JOBS; do
  if [ $dry = 1 ]; then echo "would run $job"; continue; fi
  step info "starting $job"
  case $job in
    prune) prune_old "$BACKUP_DIR" "$KEEP_DAYS" >/dev/null ;;
    *) sh "$ROOT/bin/$job.sh" || log_event warn "$job failed with status $?" nightly ;;
  esac
done
log_event info "all jobs finished at $(date +%H:%M)" nightly
END_FILE
cat >bin/rotate.sh <<'END_FILE'
#!/bin/sh
# rotate.sh FILE KEEP: rotate FILE to FILE.1 ... FILE.KEEP.
ROOT=${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$ROOT/lib/log.sh"

file=$1
keep=${2:-5}
[ -f "$file" ] || { log_event warn "nothing to rotate: $file" rotate; exit 0; }
i=$keep
while [ "$i" -gt 1 ]; do
  prev=$(( i - 1 ))
  if [ -f "$file.$prev" ]; then mv "$file.$prev" "$file.$i" || log_event error "could not move $file.$prev" rotate; fi
  i=$prev
done
mv "$file" "$file.1" && : >"$file" || log_event error "could not rotate $file" rotate
log_event info done rotate
END_FILE
cat >bin/sync-mirror.sh <<'END_FILE'
#!/bin/sh
# sync-mirror.sh NAME URL DIR MAXAGE: refresh a mirror's index and warn when it
# is older than MAXAGE hours.
ROOT=${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$ROOT/lib/log.sh"
. "$ROOT/lib/net/fetch.sh"

name=$1 url=$2 dir=$3 maxage=${4:-24}
case $name in
  ''|*/*) log_event error "bad mirror name \"$name\"" sync-mirror; exit 2 ;;
esac
msg="syncing $name from $url"
log_event info "$msg" sync-mirror
if ! fetch_url "$url/INDEX" "$dir/INDEX.new"; then
  log_event error "sync of $name failed" sync-mirror
  exit 1
fi
mv "$dir/INDEX.new" "$dir/INDEX"
age=$(cat "$dir/AGE" 2>/dev/null || echo 0)
if [ "$age" -gt "$maxage" ]; then
  log_event warn "mirror $name is stale:
last upstream change $age hours ago" sync-mirror
fi
END_FILE
cat >lib/db.sh <<'END_FILE'
# db.sh: dump a database with the configured client.

DB_DUMP=${DB_DUMP:-pg_dump}

# db_dump NAME OUT: write the dump of NAME to OUT.
db_dump() {
  # log_event debug "running $DB_DUMP $1"
  if ! command -v "$DB_DUMP" >/dev/null 2>&1; then
    log_event error "dump tool $DB_DUMP not found" db; return 1
  fi
  "$DB_DUMP" "$1" >"$2" || {
    log_event error \
      "dump of $1 failed" db
    return 1
  }
}
END_FILE
cat >lib/lock.sh <<'END_FILE'
# lock.sh: a directory lock, so two jobs do not run at once.

# lock_take DIR: create DIR or fail when another job holds it.
lock_take() {
  if mkdir "$1" 2>/dev/null; then
    echo $$ >"$1/pid"
    log_event debug "took lock $1" lock
    return 0
  fi
  log_event warn "lock $1 is held by pid $(cat "$1/pid" 2>/dev/null || echo unknown)" lock
  return 1
}

# lock_drop DIR: remove the lock again.
lock_drop() {
  rm -rf "$1"
}
END_FILE
cat >lib/log.sh <<'END_FILE'
# log.sh: leveled log lines on stderr, and helpers for reading log files.
#
# Levels, lowest first: debug, info, warn, error. LOG_LEVEL (default info)
# is the lowest level printed. When LOG_FILE is set, every printed line is
# also appended to that file.

# log_level_num LEVEL: 0 to 3 for the four levels; 9 for anything else, which
# is always printed.
log_level_num() {
  case $1 in
    debug) echo 0 ;;
    info) echo 1 ;;
    warn) echo 2 ;;
    error) echo 3 ;;
    *) echo 9 ;;
  esac
}

# log_event LEVEL MESSAGE [COMPONENT]: print "LEVEL [COMPONENT]: MESSAGE",
# or "LEVEL: MESSAGE" when COMPONENT is missing or empty, on stderr when
# LEVEL is at or above LOG_LEVEL.
log_event() {
  if [ "$(log_level_num "$1")" -lt "$(log_level_num "${LOG_LEVEL:-info}")" ]; then
    return 0
  fi
  if [ -n "${3:-}" ]; then
    _log_line="$1 [$3]: $2"
  else
    _log_line="$1: $2"
  fi
  printf '%s\n' "$_log_line" >&2
  if [ -n "${LOG_FILE:-}" ]; then
    printf '%s\n' "$_log_line" >>"$LOG_FILE"
  fi
}

# log_tail N FILE: the last N lines of a log file.
log_tail() {
  tail -n "$1" "$2"
}

# log_events FILE: how many lines of each level a log file holds, as
# "LEVEL COUNT" lines in level order. Lines with a component count too.
log_events() {
  awk '
  { level = $1; sub(/:$/, "", level) }
  level == "debug" || level == "info" || level == "warn" || level == "error" { n[level]++ }
  END {
    split("debug info warn error", order, " ")
    for (i = 1; i <= 4; i++) printf "%s %d\n", order[i], n[order[i]]
  }
  ' "$1"
}
END_FILE
cat >lib/fs/prune.sh <<'END_FILE'
# prune.sh: delete old files.

# prune_old DIR DAYS: delete regular files in DIR older than DAYS days and
# print how many went.
prune_old() {
  [ -d "$1" ] || { log_event warn "no such directory: $1" prune; echo 0; return 0; }
  gone=0
  for f in $(find "$1" -type f -mtime +"$2"); do
    rm -f "$f" && gone=$(( gone + 1 ))
  done
  log_event info "pruned $gone files older than $2 days from $1" prune
  echo "$gone"
}
END_FILE
cat >lib/net/fetch.sh <<'END_FILE'
# fetch.sh: download with retries.

FETCH_TRIES=${FETCH_TRIES:-3}

# fetch_url URL OUT: download URL to OUT, trying FETCH_TRIES times.
fetch_url() {
  n=1
  while [ "$n" -le "$FETCH_TRIES" ]; do
    if wget -q -O "$2" "$1" 2>/dev/null; then
      log_event info "fetched $1" fetch
      return 0
    fi
    log_event warn \
      "retrying $1 ($n of $FETCH_TRIES)" fetch
    n=$(( n + 1 ))
  done
  log_event error \
    "giving up on $1 after $FETCH_TRIES tries" fetch \
    || true
  return 1
}
END_FILE
