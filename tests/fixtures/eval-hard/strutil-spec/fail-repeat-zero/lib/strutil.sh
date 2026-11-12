# strutil.sh: string helpers for POSIX sh. Source this file; see
# docs/strutil.md for the contract of each function.

_strutil_tab=$(printf '\t')

# str_trim S
str_trim() {
  printf '%s\n' "$1" | sed "s/^[ $_strutil_tab]*//; s/[ $_strutil_tab]*\$//"
}

# str_pad_left S WIDTH [CHAR]
str_pad_left() {
  case $2 in
    '' | *[!0-9]*) return 2 ;;
  esac
  _strutil_char=${3- }
  [ "${#_strutil_char}" -eq 1 ] || return 2
  _strutil_out=$1
  while [ "${#_strutil_out}" -lt "$2" ]; do
    _strutil_out=$_strutil_char$_strutil_out
  done
  printf '%s\n' "$_strutil_out"
}

# str_repeat S N
str_repeat() {
  case $2 in
    '' | *[!0-9]*) return 2 ;;
  esac
  _strutil_out=$1
  _strutil_i=1
  while [ "$_strutil_i" -lt "$2" ]; do
    _strutil_out=$_strutil_out$1
    _strutil_i=$((_strutil_i + 1))
  done
  printf '%s\n' "$_strutil_out"
}

# str_count S SUB
str_count() {
  [ -n "$2" ] || return 2
  _strutil_rest=$1
  _strutil_n=0
  while :; do
    case $_strutil_rest in
      *"$2"*)
        _strutil_rest=${_strutil_rest#*"$2"}
        _strutil_n=$((_strutil_n + 1))
        ;;
      *) break ;;
    esac
  done
  printf '%s\n' "$_strutil_n"
}

# str_lower S
str_lower() {
  printf '%s\n' "$1" | tr 'ABCDEFGHIJKLMNOPQRSTUVWXYZ' 'abcdefghijklmnopqrstuvwxyz'
}

# str_starts_with S PREFIX
str_starts_with() {
  case $1 in
    "$2"*) return 0 ;;
  esac
  return 1
}
