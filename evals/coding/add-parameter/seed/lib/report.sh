# report.sh: the summary mailed after a run.

# report_header: the first lines of the summary.
report_header() {
  echo "nightly run on $(hostname)"
  echo "log lines are written by log_event LEVEL MESSAGE"
}

# report_counts FILE: the per-level counts from log_events.
report_counts() {
  log_events "$1" | while read -r level count; do
    printf '%-6s %4d\n' "$level" "$count"
  done
}
