#!/bin/sh
# order.sh FILE: validate with grep, list the names and edges, sort the
# names once, then repeatedly take the first ready name in that order.
LC_ALL=C
export LC_ALL
file=$1
name='[A-Za-z0-9._-]+'
blank='^[[:blank:]]*(#.*)?$'
rule="[[:blank:]]*$name[[:blank:]]*:[[:blank:]]*($name([[:blank:]]+$name)*)?[[:blank:]]*"
bad=$(grep -nvE "$blank" "$file" | grep -vE "^[0-9]+:$rule\$" | head -n 1)
if [ -n "$bad" ]; then
  echo "syntax error on line ${bad%%:*}" >&2
  exit 2
fi
grep -vE "$blank" "$file" | tr ':' ' ' | awk '{ print "N", $1; for (i = 2; i <= NF; i++) print "N", $i; for (i = 2; i <= NF; i++) print "E", $1, $i }' |
  sort -u | awk '
$1 == "N" { order[++count] = $2 ""; next }
{ need[$2 "", $3 ""] = 1; pending[$2 ""]++ }
END {
  printed = 0
  while (printed < count) {
    found = 0
    for (i = 1; i <= count; i++) {
      x = order[i]
      if (!(x in done) && pending[x] + 0 == 0) { found = 1; break }
    }
    if (!found) { print "cycle" | "cat >&2"; close("cat >&2"); exit 3 }
    done[x] = 1
    printed++
    out = out x "\n"
    for (j = 1; j <= count; j++) if ((order[j], x) in need) pending[order[j]]--
  }
  printf "%s", out
}'
