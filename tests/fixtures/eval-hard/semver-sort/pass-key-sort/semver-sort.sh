#!/bin/sh
# Validate with one extended regular expression, then sort on a key that
# orders bytewise the way SemVer precedence does.
LC_ALL=C
export LC_ALL
num='(0|[1-9][0-9]*)'
alnum='[0-9]*[A-Za-z-][0-9A-Za-z-]*'
pre="($num|$alnum)"
re="^$num\\.$num\\.$num(-$pre(\\.$pre)*)?(\\+[0-9A-Za-z-]+(\\.[0-9A-Za-z-]+)*)?\$"
input=$(cat; printf x)
input=${input%x}
# Zero bytes is empty input; a lone LF is one empty version.
[ -n "$input" ] || exit 0
nl='
'
input=${input%"$nl"}
first=$(printf '%s\n' "$input" | grep -nvE "$re" | head -n 1)
if [ -n "$first" ]; then
  printf 'invalid version on line %s: %s\n' "${first%%:*}" "${first#*:}" >&2
  exit 2
fi
tab=$(printf '\t')
printf '%s\n' "$input" | awk '
function pad(s) { return substr("000000000000000000000000000000", 1, 30 - length(s)) s }
{
  v = $0
  p = index(v, "+"); if (p) v = substr(v, 1, p - 1)
  p = index(v, "-")
  core = p ? substr(v, 1, p - 1) : v
  split(core, c, ".")
  key = pad(c[1]) pad(c[2]) pad(c[3])
  if (!p) key = key "~"
  else {
    n = split(substr(v, p + 1), id, ".")
    key = key "!"
    for (i = 1; i <= n; i++) {
      if (id[i] ~ /^[0-9]+$/) key = key "0" pad(id[i])
      else key = key "1" id[i]
      if (i < n) key = key " "
    }
  }
  printf "%s\t%d\t%s\n", key, NR, $0
}' | sort -t "$tab" -k1,1 -k2,2n | cut -f 3-
