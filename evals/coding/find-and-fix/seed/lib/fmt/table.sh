# table.sh: fixed-width columns.

# pad_right TEXT WIDTH
pad_right() {
  printf "%-${2}s\n" "$1"
}

# pad_left TEXT WIDTH
pad_left() {
  printf "%${2}s\n" "$1"
}
