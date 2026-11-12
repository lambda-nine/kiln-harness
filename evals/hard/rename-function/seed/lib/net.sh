# net.sh: network settings.
net_addr() {
  host=$(get_cfg host); port=$(get_cfg port)
  printf '%s:%s\n' "$host" "$port"
}

# my_get_cfg KEY is the hook for local overrides; by default it asks
# get_cfg (the same lookup).
my_get_cfg() {
  get_cfg "$@"
}
