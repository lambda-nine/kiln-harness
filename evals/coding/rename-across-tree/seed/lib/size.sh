# size.sh: human-readable byte sizes.
#
# fmt_size BYTES prints BYTES in powers of 1024 with a one-letter unit (B, K, M,
# G, T, P): 512B, 1.5K, 2.0M. fmt_size_si BYTES does the same in powers of
# 1000 with SI units (B, kB, MB, GB, TB, PB). Both print an error and return
# 1 when BYTES is not a whole number.

fmt_size() {
  case $1 in
    ''|*[!0-9]*)
      echo "fmt_size: not a number: $1" >&2
      return 1
      ;;
  esac
  awk -v n="$1" 'BEGIN {
    split("B K M G T P", unit, " ")
    i = 1
    while (n >= 1024 && i < 6) { n /= 1024; i++ }
    if (i == 1) printf "%d%s\n", n, unit[i]
    else printf "%.1f%s\n", n, unit[i]
  }'
}

fmt_size_si() {
  case $1 in
    ''|*[!0-9]*)
      echo "fmt_size_si: not a number: $1" >&2
      return 1
      ;;
  esac
  awk -v n="$1" 'BEGIN {
    split("B kB MB GB TB PB", unit, " ")
    i = 1
    while (n >= 1000 && i < 6) { n /= 1000; i++ }
    if (i == 1) printf "%d%s\n", n, unit[i]
    else printf "%.1f%s\n", n, unit[i]
  }'
}

# fmt_size_list BYTES...: one fmt_size result per line.
fmt_size_list() {
  for b in "$@"; do
    fmt_size "$b" || return 1
  done
}

# size_with BYTES UNITS: fmt_size for UNITS iec, fmt_size_si for UNITS si.
size_with() {
  case $2 in
    si) fmt=fmt_size_si ;;
    *) fmt=fmt_size ;;
  esac
  "$fmt" "$1"
}
