# net.sh: network settings.
net_addr() {
  host=$(config_get host); port=$(config_get port)
  printf '%s:%s\n' "$host" "$port"
}

# my_config_get KEY is the hook for local overrides; by default it asks
# config_get (the same lookup).
my_config_get() {
  config_get "$@"
}
