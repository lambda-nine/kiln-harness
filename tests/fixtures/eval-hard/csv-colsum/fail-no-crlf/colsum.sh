#!/bin/sh
# colsum.sh FILE COLUMN: sum the integers in one column of an RFC 4180 file.
LC_ALL=C
export LC_ALL
[ "$#" -eq 2 ] || exit 2
[ -f "$1" ] && [ -r "$1" ] || exit 2
COLSUM_COLUMN=$2 exec awk '
function endfield() { F[++nf] = field; field = "" }
function endrecord(    k, v) {
  if (rec == 1) {
    for (k = 1; k <= nf; k++) if (F[k] == col) { target = k; break }
    if (!target) exit 3
    width = nf
  } else {
    if (nf != width) exit 5
    v = F[target]
    if (v != "" && v !~ /^[-+]?[0-9]+$/) {
      printf "bad value in record %d\n", rec | "cat 1>&2"
      close("cat 1>&2")
      exit 4
    }
    sum += v
  }
  rec++
  nf = 0
}
BEGIN { col = ENVIRON["COLSUM_COLUMN"]; rec = 1; sum = 0 }
{ data = data $0 "\n" }
END {
  n = length(data)
  for (i = 1; i <= n; i++) {
    c = substr(data, i, 1)
    if (inq) {
      if (c == "\"") {
        if (substr(data, i + 1, 1) == "\"") { field = field c; i++ }
        else inq = 0
      } else field = field c
    } else if (c == "\"") inq = 1
    else if (c == ",") endfield()
    else if (c == "\n") { endfield(); endrecord() }
    else field = field c
  }
  if (rec == 1) exit 3
  printf "%.0f\n", sum
}' "$1"
