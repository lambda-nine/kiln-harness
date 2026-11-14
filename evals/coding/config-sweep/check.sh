cd /workspace || exit 2
LC_ALL=C
export LC_ALL
# Only settings.conf: no backup or temporary files.
[ "$(find . -type f)" = ./settings.conf ] || { echo "files"; exit 1; }
# The expected file, regenerated: the seed's generator with the four
# changes applied.
want=$(mktemp) || exit 2
awk -v want=1 '
BEGIN {
  split("server database cache logging auth queue metrics storage mail search", sec, " ")
  print "# settings.conf: one setting per line, written section.key_N = value."
  print "# Lines that start with # are comments, not settings."
  for (n = 1; n <= 1440; n++) {
    if (n % 60 == 1) {
      print ""
      printf "# Block %d: keys %d to %d.\n", (n - 1) / 60 + 1, n, n + 59
    }
    if (n == 311) print "# database.key_311 = 512 was the default before release 4; kept for reference."
    if (n == 555) print "# queue.key_555 is set on the next line."
    if (n == 842) print "# cache.key_842 = on   <- an example only, not a setting."
    if (n == 1204) print "# auth.key_1204 = lenient (the old value)"
    s = sec[n % 10 + 1]
    m = n % 4
    if (m == 0) v = (n * 37) % 1000
    else if (m == 1) v = (int(n / 4) % 2) ? "on" : "off"
    else if (m == 2) v = "\"instance-" n "-zone-" ((n * 7) % 13) "\""
    else v = "/var/lib/service-data/" s "/shard-" (n % 8) "/" n
    line = sprintf("%s.key_%d = %s", s, n, v)
    if (want) {
      if (n == 311) line = "database.key_311 = 4096"
      if (n == 555) continue
      if (n == 842) line = "cache.key_842 = off"
      if (n == 1204) line = "auth.key_1204 = strict"
    }
    print line
    if (n % 10 == 0 && n % 60 != 0) printf "# Keys up to %d are read at startup; later keys on reload.\n", n
  }
}
' >"$want" || { rm -f "$want"; exit 2; }
if cmp -s "$want" settings.conf; then
  rm -f "$want"
  exit 0
fi
rm -f "$want"
echo "settings.conf"
exit 1
