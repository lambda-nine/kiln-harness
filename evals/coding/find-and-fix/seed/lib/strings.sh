# strings.sh: small string helpers.

# trim TEXT: TEXT without leading and trailing blanks.
trim() {
  printf '%s\n' "$1" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//'
}

# to_upper TEXT
to_upper() {
  printf '%s\n' "$1" | tr '[:lower:]' '[:upper:]'
}

# to_lower TEXT
to_lower() {
  printf '%s\n' "$1" | tr '[:upper:]' '[:lower:]'
}

# repeat_str TEXT COUNT: TEXT repeated COUNT times.
repeat_str() {
  _rs_out=''
  _rs_i=0
  while [ "$_rs_i" -lt "$2" ]; do
    _rs_out=$_rs_out$1
    _rs_i=$((_rs_i + 1))
  done
  printf '%s\n' "$_rs_out"
}

# starts_with TEXT PREFIX
starts_with() {
  case $1 in
    "$2"*) return 0 ;;
  esac
  return 1
}

# ends_with TEXT SUFFIX
ends_with() {
  case $1 in
    *"$2") return 0 ;;
  esac
  return 1
}
