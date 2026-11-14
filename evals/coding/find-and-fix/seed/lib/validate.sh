# validate.sh: shape checks for user input.

# is_hex TEXT: true for one or more hexadecimal digits.
is_hex() {
  case $1 in
    '' | *[!0-9a-fA-F]*) return 1 ;;
  esac
  return 0
}

# is_email TEXT: a rough check, one @ with text on both sides and a dot after.
is_email() {
  case $1 in
    *@*@*) return 1 ;;
    ?*@?*.?*) return 0 ;;
  esac
  return 1
}
