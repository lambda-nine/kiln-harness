#!/bin/sh
# summary.sh LOGFILE: print the report that would be mailed.
ROOT=${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$ROOT/lib/log.sh"
. "$ROOT/lib/report.sh"

[ -f "$1" ] || { echo "usage: summary.sh LOGFILE" >&2; exit 2; }
report_header
report_counts "$1"
