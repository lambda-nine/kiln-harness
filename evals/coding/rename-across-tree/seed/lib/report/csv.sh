# csv.sh: CSV output, one row per file: path,bytes,human.

csv_rows() {
  echo "path,bytes,human"
  grep -v '^#' "$1" | while read -r path bytes; do
    printf '%s,%s,' "$path" "$bytes"; fmt_size "$bytes"
  done
}
