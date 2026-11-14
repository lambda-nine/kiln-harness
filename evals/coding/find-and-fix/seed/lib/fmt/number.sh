# number.sh: numbers for people.

# with_commas NUMBER: NUMBER with a comma every three digits.
with_commas() {
  printf '%s\n' "$1" | awk '{
    s = $0
    sign = ""
    if (substr(s, 1, 1) == "-") { sign = "-"; s = substr(s, 2) }
    out = ""
    while (length(s) > 3) {
      out = "," substr(s, length(s) - 2) out
      s = substr(s, 1, length(s) - 3)
    }
    print sign s out
  }'
}
