# fetch.sh: download with retries.

FETCH_TRIES=${FETCH_TRIES:-3}

# fetch_url URL OUT: download URL to OUT, trying FETCH_TRIES times.
fetch_url() {
  n=1
  while [ "$n" -le "$FETCH_TRIES" ]; do
    if wget -q -O "$2" "$1" 2>/dev/null; then
      log_event info "fetched $1"
      return 0
    fi
    log_event warn \
      "retrying $1 ($n of $FETCH_TRIES)"
    n=$(( n + 1 ))
  done
  log_event error \
    "giving up on $1 after $FETCH_TRIES tries" \
    || true
  return 1
}
