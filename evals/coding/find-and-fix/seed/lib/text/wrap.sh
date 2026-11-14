# wrap.sh: line wrapping.

# wrap_text WIDTH: standard input wrapped at spaces to WIDTH columns.
wrap_text() {
  fold -s -w "$1"
}
