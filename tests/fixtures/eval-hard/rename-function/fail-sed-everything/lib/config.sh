# config.sh: configuration lookup.
#
# config_get KEY prints the value of KEY, or nothing when KEY is not set.
# config_gets prints every KEY=VALUE pair, and config_get_path the file they
# would be read from. Everything else should go through config_get

# Prefix of the generated accessor names (config_get_port and so on).
CFG_PREFIX="config_get_"

CFG_DATA='name=demo
host=localhost
port=8080
retries=3'

config_get() {
  printf '%s\n' "$CFG_DATA" | while IFS='=' read -r key value; do
    if [ "$key" = "$1" ]; then printf '%s\n' "$value"; fi
  done
}

config_gets() {
  printf '%s\n' "$CFG_DATA"
}

config_get_path() {
  printf '%s\n' "${APP_CONF:-etc/app.conf}"
}
