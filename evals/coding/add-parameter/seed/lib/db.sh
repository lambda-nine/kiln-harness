# db.sh: dump a database with the configured client.

DB_DUMP=${DB_DUMP:-pg_dump}

# db_dump NAME OUT: write the dump of NAME to OUT.
db_dump() {
  # log_event debug "running $DB_DUMP $1"
  if ! command -v "$DB_DUMP" >/dev/null 2>&1; then
    log_event error "dump tool $DB_DUMP not found"; return 1
  fi
  "$DB_DUMP" "$1" >"$2" || {
    log_event error \
      "dump of $1 failed"
    return 1
  }
}
