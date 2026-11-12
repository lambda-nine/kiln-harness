#!/bin/sh
# merge.sh FILE...: merge layered INI files, later ones overriding.
LC_ALL=C
export LC_ALL
[ "$#" -ge 1 ] || exit 2
exec awk '
function trim(s) {
  sub(/^[ \t]+/, "", s)
  sub(/[ \t]+$/, "", s)
  return s
}
function addsection(s) {
  if (!(s in hassec)) { hassec[s] = 1; secs[++nsecs] = s }
}
BEGIN {
  nsecs = 0
  addsection("")
  for (a = 1; a < ARGC; a++) {
    file = ARGV[a]
    sec = ""
    lineno = 0
    while ((r = (getline line < file)) > 0) {
      lineno++
      if (line ~ /^[ \t]*$/ || line ~ /^[ \t]*[;#]/) continue
      if (line ~ /^[ \t]*\[[ \t]*[A-Za-z0-9._-]+[ \t]*\][ \t]*$/) {
        name = line
        sub(/^[ \t]*\[/, "", name)
        sub(/\][ \t]*$/, "", name)
        sec = trim(name)
        addsection(sec)
        continue
      }
      if (line ~ /^[ \t]*[A-Za-z0-9._-]+[ \t]*=/) {
        p = index(line, "=")
        key = trim(substr(line, 1, p - 1))
        value = trim(substr(line, p + 1))
        if (!((sec, key) in haskey)) {
          haskey[sec, key] = 1
          nkeys[sec]++
          keys[sec, nkeys[sec]] = key
        }
        if (value == "!unset") delete vals[sec, key]
        else vals[sec, key] = value
        continue
      }
      printf "%s:%d: malformed\n", file, lineno | "cat 1>&2"
      close("cat 1>&2")
      exit 3
    }
    if (r < 0) exit 2
    close(file)
  }
  out = ""
  for (i = 1; i <= nsecs; i++) {
    s = secs[i]
    group = ""
    for (j = 1; j <= nkeys[s]; j++) {
      k = keys[s, j]
      if ((s, k) in vals) group = group k "=" vals[s, k] "\n"
    }
    if (group == "") continue
    if (s != "") group = "[" s "]\n" group
    out = out (out == "" ? "" : "\n") group
  }
  printf "%s", out
  exit 0
}' "$@"
