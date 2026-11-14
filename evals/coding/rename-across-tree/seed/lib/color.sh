# color.sh: optional colour, off when NO_COLOR is set or stdout is not a
# terminal.

color_on() {
  [ -z "${NO_COLOR:-}" ] && [ -t 1 ]
}

bold() {
  if color_on; then printf '\033[1m%s\033[0m\n' "$1"; else printf '%s\n' "$1"; fi
}
