# prune.sh: delete old files.

# prune_old DIR DAYS: delete regular files in DIR older than DAYS days and
# print how many went.
prune_old() {
  [ -d "$1" ] || { log_event warn "no such directory: $1"; echo 0; return 0; }
  gone=0
  for f in $(find "$1" -type f -mtime +"$2"); do
    rm -f "$f" && gone=$(( gone + 1 ))
  done
  log_event info "pruned $gone files older than $2 days from $1"
  echo "$gone"
}
