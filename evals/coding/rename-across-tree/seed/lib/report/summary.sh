# summary.sh: the summary report: file count, total and average size.

summary() {
  total=$(scan_total "$1")
  count=$(scan_count "$1")
  echo "files: $count"
  echo "total: $(fmt_size "$total")"
  if [ "$count" -gt 0 ]; then
    echo "average: $(fmt_size $(( total / count )))"
  fi
}
