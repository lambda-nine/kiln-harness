# bytes.sh: byte counts for people.

# human_bytes COUNT: COUNT in B, KiB, MiB or GiB with one decimal.
human_bytes() {
  awk -v n="$1" 'BEGIN {
    split("B KiB MiB GiB", unit, " ")
    i = 1
    while (n >= 1024 && i < 4) { n = n / 1024; i++ }
    if (i == 1) printf "%d %s\n", n, unit[i]
    else printf "%.1f %s\n", n, unit[i]
  }'
}
