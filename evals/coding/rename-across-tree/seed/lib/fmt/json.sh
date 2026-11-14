# json.sh: JSON output.

# json_size NAME BYTES: {"name": NAME, "bytes": BYTES, "human": SIZE}, with
# SIZE from fmt_size.
json_size() {
  fmt_size=$(fmt_size "$2") || return 1
  printf '{"name": "%s", "bytes": %s, "human": "%s"}\n' "$1" "$2" "$fmt_size"
}

json_lines() {
  grep -v '^#' "$1" | while read -r path bytes; do
    json_size "$path" "$bytes"
  done
}
