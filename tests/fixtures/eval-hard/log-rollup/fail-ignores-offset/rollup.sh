#!/bin/sh
# rollup.sh DIR: count log levels per UTC hour over DIR's .log files.
LC_ALL=C
export LC_ALL
dir=$1
set --
for f in "$dir"/* "$dir"/.*; do
  case ${f##*/} in
    *.log) [ -f "$f" ] && set -- "$@" "$f" ;;
  esac
done
if [ "$#" -eq 0 ]; then
  echo "malformed=0"
  exit 0
fi
exec awk '
function leap(y) { return (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 }
function dim(y, m) {
  if (m == 2) return leap(y) ? 29 : 28
  return (m == 4 || m == 6 || m == 9 || m == 11) ? 30 : 31
}
/^[ \t]*$/ { next }
{
  if (index($0, "\r") || $0 !~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9](Z|[-+][0-9][0-9]:[0-9][0-9]) (DEBUG|INFO|WARN|ERROR)( .+)?$/) {
    bad++
    next
  }
  y = substr($0, 1, 4) + 0; mo = substr($0, 6, 2) + 0; d = substr($0, 9, 2) + 0
  h = substr($0, 12, 2) + 0; mi = substr($0, 15, 2) + 0; s = substr($0, 18, 2) + 0
  if (y < 2000 || y > 2099 || mo < 1 || mo > 12 || d < 1 || d > dim(y, mo) ||
      h > 23 || mi > 59 || s > 59) { bad++; next }
  off = 0
  rest = substr($0, 20)
  if (substr(rest, 1, 1) != "Z") {
    om = substr(rest, 5, 2) + 0
    if (om > 59) { bad++; next }
    off = (substr(rest, 2, 2) + 0) * 60 + om
    if (substr(rest, 1, 1) == "-") off = -off
    if (off < -720 || off > 840) { bad++; next }
    rest = substr(rest, 7)
  } else rest = substr(rest, 2)
  level = substr(rest, 2)
  sub(/ .*/, "", level)
  t = h * 60 + mi
  if (t < 0) {
    t += 1440
    if (--d < 1) {
      if (--mo < 1) { mo = 12; y-- }
      d = dim(y, mo)
    }
  } else if (t >= 1440) {
    t -= 1440
    if (++d > dim(y, mo)) {
      d = 1
      if (++mo > 12) { mo = 1; y++ }
    }
  }
  key = sprintf("%04d-%02d-%02dT%02d", y, mo, d, int(t / 60))
  if (!(key in seen)) { seen[key] = 1; keys[++nkeys] = key }
  count[key, level]++
}
END {
  for (i = 2; i <= nkeys; i++) {
    k = keys[i]
    for (j = i - 1; j >= 1 && keys[j] > k; j--) keys[j + 1] = keys[j]
    keys[j + 1] = k
  }
  for (i = 1; i <= nkeys; i++) {
    k = keys[i]
    printf "%s debug=%d info=%d warn=%d error=%d\n", k,
      count[k, "DEBUG"], count[k, "INFO"], count[k, "WARN"], count[k, "ERROR"]
  }
  printf "malformed=%d\n", bad
}' "$@"
