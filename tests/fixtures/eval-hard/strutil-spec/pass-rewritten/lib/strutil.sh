# strutil.sh: string helpers for POSIX sh, rewritten to the spec in
# docs/strutil.md with parameter expansion and awk.

_strutil_blanks=" $(printf '\t')"

str_trim() {
  _strutil_s=$1
  while :; do
    case $_strutil_s in
      ["$_strutil_blanks"]*) _strutil_s=${_strutil_s#?} ;;
      *) break ;;
    esac
  done
  while :; do
    case $_strutil_s in
      *["$_strutil_blanks"]) _strutil_s=${_strutil_s%?} ;;
      *) break ;;
    esac
  done
  printf '%s\n' "$_strutil_s"
}

_strutil_count() {
  case $1 in
    '' | *[!0-9]*) return 1 ;;
  esac
}

# _strutil_dec DIGITS: DIGITS without leading zeros, so shell arithmetic
# reads it as decimal, not octal.
_strutil_dec() {
  _strutil_d=$1
  while [ "${#_strutil_d}" -gt 1 ] && [ "${_strutil_d#0}" != "$_strutil_d" ]; do
    _strutil_d=${_strutil_d#0}
  done
  printf '%s\n' "$_strutil_d"
}

str_pad_left() {
  _strutil_count "$2" || return 2
  if [ "$#" -ge 3 ]; then
    [ "${#3}" -eq 1 ] || return 2
    _strutil_c=$3
  else
    _strutil_c=' '
  fi
  _strutil_pad=
  _strutil_k=$(($(_strutil_dec "$2") - ${#1}))
  while [ "$_strutil_k" -gt 0 ]; do
    _strutil_pad=$_strutil_pad$_strutil_c
    _strutil_k=$((_strutil_k - 1))
  done
  printf '%s%s\n' "$_strutil_pad" "$1"
}

str_repeat() {
  _strutil_count "$2" || return 2
  _strutil_k=$(_strutil_dec "$2")
  while [ "$_strutil_k" -gt 0 ]; do
    printf '%s' "$1"
    _strutil_k=$((_strutil_k - 1))
  done
  printf '\n'
}

str_count() {
  [ -n "$2" ] || return 2
  STRUTIL_S=$1 STRUTIL_SUB=$2 awk 'BEGIN {
    s = ENVIRON["STRUTIL_S"]; t = ENVIRON["STRUTIL_SUB"]; n = 0
    while ((i = index(s, t)) > 0) { n++; s = substr(s, i + length(t)) }
    print n
  }'
}

str_lower() {
  printf '%s\n' "$1" | tr '[:upper:]' '[:lower:]'
}

str_starts_with() {
  [ "${#2}" -le "${#1}" ] || return 1
  _strutil_head=$1
  while [ "${#_strutil_head}" -gt "${#2}" ]; do
    _strutil_head=${_strutil_head%?}
  done
  [ "$_strutil_head" = "$2" ]
}
