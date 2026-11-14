# hist.sh: a histogram of file sizes by power-of-two bucket. The awk
# program below defines its own helper of the same name for the bucket
# labels; it does not call the shell function.

hist() {
  awk '
  function fmt_size(n,   i, u) {
    split("B K M G T P", u, " ")
    i = 1
    while (n >= 1024 && i < 6) { n /= 1024; i++ }
    return sprintf("%d%s", n, u[i])
  }
  !/^#/ && NF == 2 {
    b = 1
    while (b < $2) b *= 2
    count[b]++
    if (b > max) max = b
  }
  END {
    for (b = 1; b <= max; b *= 2)
      if (b in count) printf "%6s %d\n", "<=" fmt_size(b), count[b]
  }
  ' "$1"
}
