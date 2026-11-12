#!/bin/sh
# rollup.sh DIR: convert each timestamp to minutes since 1970 and back.
LC_ALL=C
export LC_ALL
cd "$1" 2>/dev/null || { echo "malformed=0"; exit 0; }
ls -A | awk '
function days(y, m, d,    era, yoe, doy, doe) {
  # Days since 1970-01-01 of a Gregorian date.
  if (m <= 2) y--
  era = int(y / 400)
  yoe = y - era * 400
  doy = int((153 * (m + (m > 2 ? -3 : 9)) + 2) / 5) + d - 1
  doe = yoe * 365 + int(yoe / 4) - int(yoe / 100) + doy
  return era * 146097 + doe - 719468
}
function civil(z,    era, doe, yoe, doy, mp) {
  z += 719468
  era = int(z / 146097)
  doe = z - era * 146097
  yoe = int((doe - int(doe / 1460) + int(doe / 36524) - int(doe / 146096)) / 365)
  doy = doe - (365 * yoe + int(yoe / 4) - int(yoe / 100))
  mp = int((5 * doy + 2) / 153)
  D = doy - int((153 * mp + 2) / 5) + 1
  M = mp < 10 ? mp + 3 : mp - 9
  Y = yoe + era * 400 + (M <= 2)
}
function line(s,    y, m, d, z, off, sign, mins, lvl) {
  if (s ~ /^[ \t]*$/) return
  if (s ~ /\r/ || s !~ /^[0-9][0-9][0-9][0-9]-[01][0-9]-[0-3][0-9]T[0-2][0-9]:[0-5][0-9]:[0-5][0-9](Z|[-+][01][0-9]:[0-5][0-9]) (DEBUG|INFO|WARN|ERROR)( .+)?$/) { bad++; return }
  y = substr(s, 1, 4) + 0; m = substr(s, 6, 2) + 0; d = substr(s, 9, 2) + 0
  if (y < 2000 || y > 2099 || m < 1 || m > 12 || d < 1 || substr(s, 12, 2) + 0 > 23) { bad++; return }
  # A date is real when it survives the round trip.
  civil(days(y, m, d))
  if (Y != y || M != m || D != d) { bad++; return }
  off = 0
  if (substr(s, 20, 1) != "Z") {
    sign = substr(s, 20, 1) == "-" ? -1 : 1
    off = sign * (substr(s, 21, 2) * 60 + substr(s, 24, 2))
    if (off < -720 || off > 840) { bad++; return }
  }
  mins = days(y, m, d) * 1440 + substr(s, 12, 2) * 60 + substr(s, 15, 2) - off
  z = mins >= 0 ? int(mins / 1440) : -int((-mins + 1439) / 1440)
  civil(z)
  key = sprintf("%04d-%02d-%02dT%02d", Y, M, D, int((mins - z * 1440) / 60))
  lvl = s
  sub(/^[^ ]* /, "", lvl)
  sub(/ .*/, "", lvl)
  hours[key] = 1
  n[key " " lvl]++
}
{
  name = $0
  if (name !~ /\.log$/) next
  if (system("test -f \"" name "\"") != 0) next
  while ((r = (getline s < name)) > 0) line(s)
  close(name)
}
END {
  for (k in hours)
    printf "%s debug=%d info=%d warn=%d error=%d\n", k, n[k " DEBUG"], n[k " INFO"], n[k " WARN"], n[k " ERROR"] | "sort"
  close("sort")
  printf "malformed=%d\n", bad
}'
