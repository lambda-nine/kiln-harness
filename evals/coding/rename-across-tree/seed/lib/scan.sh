# scan.sh: read scan files of "PATH BYTES" lines. Lines that start with #
# are comments.

scan_total() {
  awk '!/^#/ && NF == 2 { t += $2 } END { printf "%d\n", t }' "$1"
}

scan_count() {
  awk '!/^#/ && NF == 2 { n++ } END { printf "%d\n", n }' "$1"
}

scan_largest() {
  awk '!/^#/ && NF == 2 && $2 >= m { m = $2; p = $1 } END { print p, m + 0 }' "$1"
}
