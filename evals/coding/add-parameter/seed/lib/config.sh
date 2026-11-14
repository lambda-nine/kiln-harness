# config.sh: read KEY=VALUE files without running them.

# config_get FILE KEY: the value of KEY, without quotes.
config_get() {
  awk -F= -v k="$2" '$1 == k { v = substr($0, length(k) + 2); gsub(/^"|"$/, "", v); print v; exit }' "$1"
}
