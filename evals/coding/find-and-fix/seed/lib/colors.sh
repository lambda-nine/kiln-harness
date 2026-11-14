# colors.sh: optional terminal colors. NO_COLOR turns them off.

# color_wrap CODE TEXT: TEXT in the ANSI color CODE, unless NO_COLOR is set.
color_wrap() {
  if [ -n "${NO_COLOR:-}" ]; then
    printf '%s\n' "$2"
  else
    printf '\033[%sm%s\033[0m\n' "$1" "$2"
  fi
}

red() { color_wrap 31 "$1"; }
green() { color_wrap 32 "$1"; }
