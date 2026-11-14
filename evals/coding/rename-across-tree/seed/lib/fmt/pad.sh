# pad.sh: padding helpers.

pad_left() {
  printf "%$2s\n" "$1"
}

pad_right() {
  printf "%-$2s|\n" "$1" | sed 's/|$//'
}
