# lines.sh: reading lines of files.

# nth_line N FILE: line N of FILE, from 1.
nth_line() {
  sed -n "$1{p;q;}" "$2"
}

# line_count FILE
line_count() {
  wc -l <"$1" | tr -d ' '
}
