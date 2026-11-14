#!/bin/sh
# dus.sh: disk usage reports from a scan file of "PATH BYTES" lines.
DUS_ROOT=${DUS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
export DUS_ROOT
for lib in util size scan fmt/table fmt/json report/summary report/top awk/hist; do
  . "$DUS_ROOT/lib/$lib.sh"
done

usage() {
  cat <<EOF
usage: dus.sh summary FILE | top N FILE | hist FILE | json FILE
Sizes are printed as fmt_size prints them, in powers of 1024.
EOF
}

case $1 in
  summary) [ $# -eq 2 ] || { usage >&2; exit 2; }; summary "$2" ;;
  top) is_uint "$2" || die "top: not a number: $2"; top_n "$3" "$2" ;;
  hist) hist "$2" ;;
  json) json_lines "$2" ;;
  largest) set -- $(scan_largest "$2"); echo "$1 $(fmt_size "$2")" ;;
  *) usage >&2; exit 2 ;;
esac
