# log.sh: logging. The level comes from get_cfg, defaulting to info.
log_level() {
  level=$(get_cfg log_level)
  printf '%s\n' "${level:-info}"
}

log_msg() {
  printf '[%s] %s\n' "$(log_level)" "$*"
}
