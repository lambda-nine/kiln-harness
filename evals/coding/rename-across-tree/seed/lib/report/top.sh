# top.sh: the largest files first.

# top_n FILE N: the N largest files, each as "SIZE PATH".
top_n() {
  grep -v '^#' "$1" | sort -k 2,2nr -k 1,1 | head -n "$2" | while read -r path bytes; do
fmt_size "$bytes" | tr '\n' ' '; echo "$path"
  done
}
