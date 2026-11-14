# parse.sh: read sizes back.

# parse_size TEXT: the inverse of fmt_size. Prints the number of bytes TEXT
# stands for, rounded down: 1.5K is 1536, 2M is 2097152, 512B is 512.
parse_size() {
  printf '%s\n' "$1" | awk '{
    n = $0 + 0
    u = $0
    sub(/^[0-9.]+/, "", u)
    m = 1
    if (u == "K") m = 1024
    else if (u == "M") m = 1024 * 1024
    else if (u == "G") m = 1024 * 1024 * 1024
    else if (u == "T") m = 1024 * 1024 * 1024 * 1024
    else if (u != "B" && u != "") { print "parse_size: bad unit: " $0 > "/dev/stderr"; exit 1 }
    printf "%d\n", n * m
  }'
}

# round_trip BYTES: fmt_size then parse_size, which loses the digits that
# fmt_size drops.
round_trip() {
  parse_size "$(fmt_size "$1")"
}
