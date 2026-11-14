# log.sh: messages on stderr.

log() {
  printf '%s\n' "$*" >&2
}

# log_size LABEL BYTES: LABEL and BYTES as fmt_size prints it.
log_size() {
  printf '%s: %s\n' "$1" "$(fmt_size "$2")" >&2
}
