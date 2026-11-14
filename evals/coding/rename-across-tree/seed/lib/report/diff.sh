# diff.sh: compare two scans.

# size_delta NEW OLD: "+SIZE" or "-SIZE", the change as fmt_size prints it.
size_delta() {
  if [ "$1" -ge "$2" ]; then
    echo "+$(fmt_size $(( $1 - $2 )))"
  else
    echo "-$(fmt_size $(( $2 - $1 )))"
  fi
}

# scan_diff NEW OLD: the change in total size between two scan files.
scan_diff() {
  size_delta "$(scan_total "$1")" "$(scan_total "$2")"
}
