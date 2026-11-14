# util.sh: small helpers shared by the commands.

die() {
  echo "dus: $*" >&2
  exit 1
}

is_uint() {
  case $1 in
    ''|*[!0-9]*) return 1 ;;
  esac
  return 0
}

# need_sizes: load size.sh unless fmt_size is already defined.
need_sizes() {
  command -v fmt_size >/dev/null 2>&1 || . "$DUS_ROOT/lib/size.sh"
}
