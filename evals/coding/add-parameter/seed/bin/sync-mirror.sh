#!/bin/sh
# sync-mirror.sh NAME URL DIR MAXAGE: refresh a mirror's index and warn when it
# is older than MAXAGE hours.
ROOT=${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$ROOT/lib/log.sh"
. "$ROOT/lib/net/fetch.sh"

name=$1 url=$2 dir=$3 maxage=${4:-24}
case $name in
  ''|*/*) log_event error "bad mirror name \"$name\""; exit 2 ;;
esac
msg="syncing $name from $url"
log_event info "$msg"
if ! fetch_url "$url/INDEX" "$dir/INDEX.new"; then
  log_event error "sync of $name failed"
  exit 1
fi
mv "$dir/INDEX.new" "$dir/INDEX"
age=$(cat "$dir/AGE" 2>/dev/null || echo 0)
if [ "$age" -gt "$maxage" ]; then
  log_event warn "mirror $name is stale:
last upstream change $age hours ago"
fi
