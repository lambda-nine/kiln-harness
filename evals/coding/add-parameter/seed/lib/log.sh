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

# log_event LEVEL MESSAGE: print "LEVEL: MESSAGE" on stderr when LEVEL is at
# or above LOG_LEVEL.
log_event() {
  if [ "$(log_level_num "$1")" -lt "$(log_level_num "${LOG_LEVEL:-info}")" ]; then
    return 0
  fi
  _log_line="$1: $2"
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
