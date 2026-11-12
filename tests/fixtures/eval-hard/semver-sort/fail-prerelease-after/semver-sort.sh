#!/bin/sh
# Sort versions read on stdin by Semantic Versioning 2.0.0 precedence.
LC_ALL=C
export LC_ALL
exec awk '
function numok(s) { return s ~ /^(0|[1-9][0-9]*)$/ }
function idok(s) { return s ~ /^[0-9A-Za-z-]+$/ }
function valid(v,    core, pre, build, p, n, i, parts) {
  build = ""; pre = ""; core = v
  p = index(core, "+")
  if (p > 0) {
    build = substr(core, p + 1); core = substr(core, 1, p - 1)
    n = split(build, parts, ".")
    if (build == "" || substr(build, length(build), 1) == ".") return 0
    for (i = 1; i <= n; i++) if (!idok(parts[i])) return 0
  }
  p = index(core, "-")
  if (p > 0) {
    pre = substr(core, p + 1); core = substr(core, 1, p - 1)
    if (pre == "" || substr(pre, length(pre), 1) == ".") return 0
    n = split(pre, parts, ".")
    for (i = 1; i <= n; i++) {
      if (!idok(parts[i])) return 0
      if (parts[i] ~ /^[0-9]+$/ && !numok(parts[i])) return 0
    }
  }
  n = split(core, parts, ".")
  if (n != 3 || substr(core, length(core), 1) == ".") return 0
  for (i = 1; i <= 3; i++) if (!numok(parts[i])) return 0
  MAJ[NR] = parts[1] ""; MIN[NR] = parts[2] ""; PAT[NR] = parts[3] ""
  HASPRE[NR] = (p > 0); PRE[NR] = pre ""
  return 1
}
# Compare two digit strings without leading zeros as numbers.
function numcmp(a, b) {
  if (length(a) != length(b)) return length(a) < length(b) ? -1 : 1
  if ((a "") == (b "")) return 0
  return (a "") < (b "") ? -1 : 1
}
function idcmp(a, b,    an, bn) {
  an = (a ~ /^[0-9]+$/); bn = (b ~ /^[0-9]+$/)
  if (an && bn) return numcmp(a, b)
  if (an) return -1
  if (bn) return 1
  if ((a "") == (b "")) return 0
  return (a "") < (b "") ? -1 : 1
}
function cmp(i, j,    c, na, nb, x, y, k) {
  if ((c = numcmp(MAJ[i], MAJ[j])) != 0) return c
  if ((c = numcmp(MIN[i], MIN[j])) != 0) return c
  if ((c = numcmp(PAT[i], PAT[j])) != 0) return c
  if (!HASPRE[i] && !HASPRE[j]) return 0
  if (!HASPRE[i]) return -1
  if (!HASPRE[j]) return 1
  na = split(PRE[i], x, "."); nb = split(PRE[j], y, ".")
  for (k = 1; k <= na && k <= nb; k++)
    if ((c = idcmp(x[k] "", y[k] "")) != 0) return c
  if (na == nb) return 0
  return na < nb ? -1 : 1
}
{
  LINE[NR] = $0
  if (!bad && !valid($0)) bad = NR
}
END {
  if (bad) {
    printf "invalid version on line %d: %s\n", bad, LINE[bad] | "cat 1>&2"
    close("cat 1>&2")
    exit 2
  }
  # Stable insertion sort of the line numbers.
  for (i = 1; i <= NR; i++) {
    ORD[i] = i
    for (j = i; j > 1 && cmp(ORD[j - 1], i) > 0; j--) ORD[j] = ORD[j - 1]
    ORD[j] = i
  }
  for (i = 1; i <= NR; i++) print LINE[ORD[i]]
}'
