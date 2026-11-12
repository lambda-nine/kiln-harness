#!/bin/sh
# colsum.sh FILE COLUMN: sum one column of a CSV file.
[ "$#" -eq 2 ] || exit 2
[ -f "$1" ] && [ -r "$1" ] || exit 2
COL=$2 awk -F, '
function unq(s) { sub(/\r$/, "", s); if (s ~ /^".*"$/) { s = substr(s, 2, length(s) - 2); gsub(/""/, "\"", s) } return s }
NR == 1 {
  for (k = 1; k <= NF; k++) if (unq($k) == ENVIRON["COL"]) { t = k; break }
  if (!t) exit 3
  w = NF
  next
}
{
  if (NF != w) { bad = 5; exit 5 }
  v = unq($t)
  if (v != "" && v !~ /^[-+]?[0-9]+$/) { printf "bad value in record %d\n", NR | "cat 1>&2"; close("cat 1>&2"); bad = 4; exit 4 }
  s += v
}
END { if (bad || !t) exit; printf "%.0f\n", s }' "$1"
