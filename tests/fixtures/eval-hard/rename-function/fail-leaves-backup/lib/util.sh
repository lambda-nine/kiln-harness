# util.sh: small helpers built on the configuration.
retries() {
  n=$(config_get retries)
  printf '%s\n' "${n:-1}"
}

cfg_count() {
  get_cfgs | wc -l | tr -d ' '
}

require() { [ -n "$1" ] || return 1; config_get "$1"; }

accessor_name() {
  printf '%s%s\n' "$CFG_PREFIX" "$1"
}
