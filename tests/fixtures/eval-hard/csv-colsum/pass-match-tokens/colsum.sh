#!/bin/sh
# colsum.sh FILE COLUMN: sum one column of a CSV file. Each field is taken
# with one regular-expression match, then its delimiter is consumed.
LC_ALL=C
export LC_ALL
if [ "$#" -ne 2 ] || [ ! -f "$1" ] || [ ! -r "$1" ]; then
  exit 2
fi
WANT=$2 awk '
{ text = text $0 "\n" }
END {
  want = ENVIRON["WANT"]
  record = 0
  while (text != "") {
    record++
    count = 0
    for (;;) {
      if (match(text, /^"([^"]|"")*"/)) {
        value = substr(text, 2, RLENGTH - 2)
        gsub(/""/, "\"", value)
      } else {
        match(text, /^[^,\n]*/)
        value = substr(text, 1, RLENGTH)
        if (substr(text, RLENGTH + 1, 1) == "\n") sub(/\r$/, "", value)
      }
      text = substr(text, RLENGTH + 1)
      field[++count] = value
      if (substr(text, 1, 2) == "\r\n") text = substr(text, 2)
      sep = substr(text, 1, 1)
      text = substr(text, 2)
      if (sep != ",") break
    }
    if (record == 1) {
      for (k = count; k >= 1; k--) if (field[k] == want) col = k
      if (!col) exit 3
      fields = count
      continue
    }
    if (count != fields) exit 5
    if (field[col] !~ /^([-+]?[0-9]+)?$/) {
      print "bad value in record " record | "cat >&2"
      close("cat >&2")
      exit 4
    }
    total += field[col]
  }
  if (!col) exit 3
  printf "%.0f\n", total
}' "$1"
