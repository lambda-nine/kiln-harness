# config.sh: read key=value files.

# config_get FILE KEY: the value of the last KEY= line, without the key.
config_get() {
  awk -v key="$2" '
    index($0, key "=") == 1 { value = substr($0, length(key) + 2); found = 1 }
    END { if (found) print value; else exit 1 }
  ' "$1"
}

# config_has FILE KEY
config_has() {
  config_get "$1" "$2" >/dev/null
}
