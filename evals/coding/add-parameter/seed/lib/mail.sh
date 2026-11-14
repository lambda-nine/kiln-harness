# mail.sh: mail the run summary.

MAIL_TO=${MAIL_TO:-root}

# mail_summary LOGFILE: mail the report for LOGFILE to MAIL_TO.
mail_summary() {
  { report_header; report_counts "$1"; } | mail -s "nightly: $(hostname)" "$MAIL_TO"
}
