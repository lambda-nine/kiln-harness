# core.sh: helpers every module and command may use.

# die MESSAGE: print MESSAGE on stderr and exit 1.
die() {
  printf 'textkit: %s\n' "$1" >&2
  exit 1
}

# is_int VALUE: true when VALUE is digits with an optional leading minus.
is_int() {
  case $1 in
    '' | - | *[!0-9-]* | ?*-*) return 1 ;;
  esac
  return 0
}

# need_args COUNT GIVEN USAGE: die with USAGE unless GIVEN is COUNT.
need_args() {
  [ "$1" -eq "$2" ] || die "usage: $3"
}
