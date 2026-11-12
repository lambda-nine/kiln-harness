# net.sh: network settings.
net_addr() {
  host=$(config_get host); port=$(config_get port)
  printf '%s:%s\n' "$host" "$port"
}

# my_get_cfg KEY is the hook for local overrides; by default it asks
# config_get (the same lookup).
my_get_cfg() {
  config_get "$@"
}
