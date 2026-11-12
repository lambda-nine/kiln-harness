# config.sh: configuration lookup.
#
# config_get KEY prints the value of KEY, or nothing when KEY is not set.
# get_cfgs prints every KEY=VALUE pair, and get_cfg_path the file they
# would be read from. Everything else should go through config_get

# Prefix of the generated accessor names (get_cfg_port and so on).
CFG_PREFIX="get_cfg_"

CFG_DATA='name=demo
host=localhost
port=8080
retries=3'

config_get() {
  printf '%s\n' "$CFG_DATA" | while IFS='=' read -r key value; do
    if [ "$key" = "$1" ]; then printf '%s\n' "$value"; fi
  done
}

get_cfgs() {
  printf '%s\n' "$CFG_DATA"
}

get_cfg_path() {
  printf '%s\n' "${APP_CONF:-etc/app.conf}"
}
