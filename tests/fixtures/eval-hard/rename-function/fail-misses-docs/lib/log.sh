# log.sh: logging. The level comes from config_get, defaulting to info.
log_level() {
  level=$(config_get log_level)
  printf '%s\n' "${level:-info}"
}

log_msg() {
  printf '[%s] %s\n' "$(log_level)" "$*"
}
