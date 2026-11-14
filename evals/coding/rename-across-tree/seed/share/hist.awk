# hist.awk: count files by power-of-two size bucket.
# Usage: awk -f share/hist.awk scan.txt
function fmt_size(n,   i, u) {
  split("B K M G T P", u, " ")
  i = 1
  while (n >= 1024 && i < 6) { n /= 1024; i++ }
  return sprintf("%d%s", n, u[i])
}
{
  b = 1
  while (b < $2) b *= 2
  count[b]++
  if (b > max) max = b
}
END {
  for (b = 1; b <= max; b *= 2)
    if (b in count) printf "%6s %d\n", "<=" fmt_size(b), count[b]
}
