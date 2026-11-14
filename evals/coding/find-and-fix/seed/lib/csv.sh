# csv.sh: simple comma-separated values, without quoting.

# csv_field LINE N: the Nth field of LINE, from 1.
csv_field() {
  printf '%s\n' "$1" | cut -d , -f "$2"
}

# csv_count LINE: how many fields LINE has.
csv_count() {
  printf '%s\n' "$1" | awk -F , '{ print NF }'
}
