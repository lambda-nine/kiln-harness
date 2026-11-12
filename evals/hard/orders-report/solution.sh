# Build /workspace/report.txt from the data, following docs/REPORT_RULES.md.
cd /workspace || exit 1
LC_ALL=C
export LC_ALL
awk -F, '
function day(s,    y, m, d) {
  # Days since 0000-03-01 in the proleptic Gregorian calendar.
  y = substr(s, 1, 4) + 0; m = substr(s, 6, 2) + 0; d = substr(s, 9, 2) + 0
  if (m <= 2) { y--; m += 12 }
  return 365 * y + int(y / 4) - int(y / 100) + int(y / 400) + int((153 * (m - 3) + 2) / 5) + d - 1
}
function dollars(c) { return sprintf("%d.%02d", int(c / 100), c % 100) }
FILENAME ~ /sku_aliases/ { split($0, pair, " "); alias[toupper(pair[1])] = toupper(pair[2]); next }
FNR == 1 { next }
FILENAME ~ /orders/ {
  status = tolower($6)
  if (status == "test" || status == "cancelled") next
  sku = toupper($3)
  if (sku in alias) sku = alias[sku]
  osku[$1] = sku; odate[$1] = day($2); ounits[$1] = $4 + 0; oprice[$1] = $5 + 0
  next
}
FILENAME ~ /returns/ {
  if (!($1 in osku)) next
  gap = day($2) - odate[$1]
  if (gap >= 0 && gap <= 30) returned[$1] += $3
}
END {
  for (id in osku) {
    net = ounits[id] - returned[id]
    if (net < 0) net = 0
    units[osku[id]] += net
    cents[osku[id]] += net * oprice[id]
  }
  n = 0
  for (s in units) if (units[s] > 0) list[++n] = s
  for (i = 2; i <= n; i++) {
    s = list[i]
    for (j = i - 1; j >= 1 && (cents[list[j]] < cents[s] || (cents[list[j]] == cents[s] && list[j] > s)); j--)
      list[j + 1] = list[j]
    list[j + 1] = s
  }
  printf "sku\tunits\trevenue\n"
  for (i = 1; i <= n; i++) {
    s = list[i]
    printf "%s\t%d\t%s\n", s, units[s], dollars(cents[s])
    tu += units[s]; tc += cents[s]
  }
  printf "TOTAL\t%d\t%s\n", tu, dollars(tc)
}' data/sku_aliases.txt data/orders.csv data/returns.csv >report.txt
