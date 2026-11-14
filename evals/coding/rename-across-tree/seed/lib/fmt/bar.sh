# bar.sh: text bar charts.

# bar WIDTH VALUE MAX: a bar of # characters, VALUE/MAX of WIDTH.
bar() {
  n=$(( $1 * $2 / $3 ))
  out=
  while [ "$n" -gt 0 ]; do out="$out#"; n=$(( n - 1 )); done
  printf '%s\n' "$out"
}

# bar_chart FILE: one bar per scan line, scaled to the largest file.
bar_chart() {
  max=$(scan_largest "$1" | cut -d ' ' -f 2)
  fmt_size $max | sed 's/^/max: /'
  grep -v '^#' "$1" | while read -r path bytes; do
    printf '%-20s %s\n' "$path" "$(bar 30 "$bytes" "$max")"
  done
}
