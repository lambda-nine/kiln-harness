# log.sh: leveled messages on stderr. LOG_LEVEL is debug, info, warn or error.

LOG_LEVEL=${LOG_LEVEL:-info}

_log_rank() {
  case $1 in
    debug) echo 0 ;;
    info) echo 1 ;;
    warn) echo 2 ;;
    error) echo 3 ;;
    *) echo 1 ;;
  esac
}

# log_at LEVEL MESSAGE
log_at() {
  if [ "$(_log_rank "$1")" -ge "$(_log_rank "$LOG_LEVEL")" ]; then
    printf '[%s] %s\n' "$1" "$2" >&2
  fi
}

log_info() { log_at info "$1"; }
log_warn() { log_at warn "$1"; }
log_error() { log_at error "$1"; }
