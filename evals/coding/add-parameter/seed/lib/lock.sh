# lock.sh: a directory lock, so two jobs do not run at once.

# lock_take DIR: create DIR or fail when another job holds it.
lock_take() {
  if mkdir "$1" 2>/dev/null; then
    echo $$ >"$1/pid"
    log_event debug "took lock $1"
    return 0
  fi
  log_event warn "lock $1 is held by pid $(cat "$1/pid" 2>/dev/null || echo unknown)"
  return 1
}

# lock_drop DIR: remove the lock again.
lock_drop() {
  rm -rf "$1"
}
