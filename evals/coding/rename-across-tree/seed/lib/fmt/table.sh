# table.sh: fixed-width rows.

# table_row NAME BYTES: NAME padded to 24 columns, then the size
# right-aligned in 8.
table_row() {
  printf '%-24s %8s\n' "$1" "$(fmt_size "$2")"
}

# table_rows: table_row for each "NAME BYTES" line on stdin.
table_rows() {
  while read -r name bytes; do
    s=`fmt_size $bytes`; printf '%-24s %8s\n' "$name" "$s"
  done
}
