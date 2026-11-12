#!/bin/sh
# order.sh FILE: a depth-first post-order over the sorted names, each
# name's dependencies visited in sorted order.
LC_ALL=C
export LC_ALL
awk '
/^[ \t]*$/ || /^[ \t]*#/ { next }
!/^[ \t]*[A-Za-z0-9._-]+[ \t]*:[ \t]*([A-Za-z0-9._-]+([ \t]+[A-Za-z0-9._-]+)*)?[ \t]*$/ {
  print "syntax error on line " NR | "cat 1>&2"; bad = 1; exit 2
}
{
  p = index($0, ":"); t = substr($0, 1, p - 1); gsub(/[ \t]/, "", t)
  print "N " t
  n = split(substr($0, p + 1), d)
  for (i = 1; i <= n; i++) print "E " t " " d[i]
}
END { if (bad) exit 2 }' "$1" >/dev/null 2>&1 || {
  awk '
/^[ \t]*$/ || /^[ \t]*#/ { next }
!/^[ \t]*[A-Za-z0-9._-]+[ \t]*:[ \t]*([A-Za-z0-9._-]+([ \t]+[A-Za-z0-9._-]+)*)?[ \t]*$/ { print "syntax error on line " NR; exit }' "$1" >&2
  exit 2
}
awk '
/^[ \t]*$/ || /^[ \t]*#/ { next }
{
  p = index($0, ":"); t = substr($0, 1, p - 1); gsub(/[ \t]/, "", t)
  print t
  n = split(substr($0, p + 1), d)
  for (i = 1; i <= n; i++) print t " " d[i]
}' "$1" | sort -u | awk '
NF == 1 { names[++count] = $1; known[$1] = 1; next }
{ if (!($2 in known)) { names[++count] = $2; known[$2] = 1 }; deps[$1] = deps[$1] " " $2 }
function visit(x,    k, i, list) {
  if (state[x] == 2) return 1
  if (state[x] == 1) return 0
  state[x] = 1
  k = split(deps[x], list, " ")
  for (i = 1; i <= k; i++) if (!visit(list[i])) return 0
  state[x] = 2
  out = out x "\n"
  return 1
}
END {
  # names[] is not sorted when a name appears only as a dependency.
  for (i = 1; i <= count; i++)
    for (j = i + 1; j <= count; j++)
      if ((names[j] "") < (names[i] "")) { s = names[i]; names[i] = names[j]; names[j] = s }
  for (i = 1; i <= count; i++)
    if (!visit(names[i])) { print "cycle" | "cat 1>&2"; close("cat 1>&2"); exit 3 }
  printf "%s", out
}'
