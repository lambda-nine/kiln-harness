cd /workspace || exit 1
cat >bin/compare.sh <<'END_FILE'
#!/bin/sh
# compare.sh NEW OLD: how much two scans differ in total, and the largest file
# of each.
DUS_ROOT=${DUS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
for lib in util size scan report/diff; do . "$DUS_ROOT/lib/$lib.sh"; done

[ $# -eq 2 ] || die "usage: compare.sh NEW OLD"
echo "change: $(scan_diff "$1" "$2")"
for f in "$1" "$2"; do
  set -- $(scan_largest "$f")
  echo "largest in $f: $1 (`format_size $2`)"
done
END_FILE
cat >bin/dus.sh <<'END_FILE'
#!/bin/sh
# dus.sh: disk usage reports from a scan file of "PATH BYTES" lines.
DUS_ROOT=${DUS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
export DUS_ROOT
for lib in util size scan fmt/table fmt/json report/summary report/top awk/hist; do
  . "$DUS_ROOT/lib/$lib.sh"
done

usage() {
  cat <<EOF
usage: dus.sh summary FILE | top N FILE | hist FILE | json FILE
Sizes are printed as fmt_size prints them, in powers of 1024.
EOF
}

case $1 in
  summary) [ $# -eq 2 ] || { usage >&2; exit 2; }; summary "$2" ;;
  top) is_uint "$2" || die "top: not a number: $2"; top_n "$3" "$2" ;;
  hist) hist "$2" ;;
  json) json_lines "$2" ;;
  largest) set -- $(scan_largest "$2"); echo "$1 $(format_size "$2")" ;;
  *) usage >&2; exit 2 ;;
esac
END_FILE
cat >bin/quota.sh <<'END_FILE'
#!/bin/sh
# quota.sh USED LIMIT: usage against a limit, both in bytes.
DUS_ROOT=${DUS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$DUS_ROOT/lib/util.sh"
. "$DUS_ROOT/lib/size.sh"
. "$DUS_ROOT/lib/quota.sh"

is_uint "$1" && is_uint "$2" || die "usage: quota.sh USED LIMIT"
cat <<EOF
used:   $(format_size "$1")
limit:  $(format_size "$2")
left:   $(quota_left "$1" "$2")
status: $(quota_check "$1" "$2")
EOF
END_FILE
cat >bin/sizeof.sh <<'END_FILE'
#!/bin/sh
# sizeof.sh: print byte counts as sizes.
DUS_ROOT=${DUS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$DUS_ROOT/lib/size.sh"

usage() {
  cat <<EOF
usage: sizeof.sh [-s] BYTES...
Prints each BYTES as fmt_size does (powers of 1024), or with -s as
fmt_size_si does (powers of 1000).
EOF
}

[ $# -gt 0 ] || { usage >&2; exit 2; }
si=0
if [ "$1" = -s ]; then si=1; shift; fi
for b in "$@"; do
  if [ $si = 1 ]; then fmt_size_si "$b"; else format_size "$b"; fi || exit 1
done
END_FILE
cat >bin/topn.sh <<'END_FILE'
#!/bin/sh
# topn.sh N FILE: the sizes of the N largest files, largest first, one per
# line.
DUS_ROOT=${DUS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
export DUS_ROOT
grep -v '^#' "$2" | awk '{ print $2 }' | sort -nr | head -n "$1" |
  xargs -n 1 sh -c '. "$DUS_ROOT/lib/size.sh"; format_size "$1"' sh
END_FILE
cat >contrib/explain.sh <<'END_FILE'
#!/bin/sh
# explain.sh BYTES...: show how each count is formatted, for bug reports.
DUS_ROOT=${DUS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$DUS_ROOT/lib/size.sh"
for b in "$@"; do
  printf 'fmt_size(%s) = %s\n' "$b" "$(format_size "$b")"
  printf 'fmt_size_si(%s) = %s\n' "$b" "$(fmt_size_si "$b")"
done
END_FILE
cat >lib/log.sh <<'END_FILE'
# log.sh: messages on stderr.

log() {
  printf '%s\n' "$*" >&2
}

# log_size LABEL BYTES: LABEL and BYTES as format_size prints it.
log_size() {
  printf '%s: %s\n' "$1" "$(format_size "$2")" >&2
}
END_FILE
cat >lib/parse.sh <<'END_FILE'
# parse.sh: read sizes back.

# parse_size TEXT: the inverse of format_size. Prints the number of bytes TEXT
# stands for, rounded down: 1.5K is 1536, 2M is 2097152, 512B is 512.
parse_size() {
  printf '%s\n' "$1" | awk '{
    n = $0 + 0
    u = $0
    sub(/^[0-9.]+/, "", u)
    m = 1
    if (u == "K") m = 1024
    else if (u == "M") m = 1024 * 1024
    else if (u == "G") m = 1024 * 1024 * 1024
    else if (u == "T") m = 1024 * 1024 * 1024 * 1024
    else if (u != "B" && u != "") { print "parse_size: bad unit: " $0 > "/dev/stderr"; exit 1 }
    printf "%d\n", n * m
  }'
}

# round_trip BYTES: format_size then parse_size, which loses the digits that
# format_size drops.
round_trip() {
  parse_size "$(format_size "$1")"
}
END_FILE
cat >lib/quota.sh <<'END_FILE'
# quota.sh: compare usage with a limit, both in bytes.

# quota_check USED LIMIT: prints "ok", or "over by SIZE" where SIZE is what
# format_size prints for the excess, and then returns 1.
quota_check() {
  if [ "$1" -le "$2" ]; then echo ok; return 0; fi
  over=$(( $1 - $2 )); echo "over by $(format_size $over)"; return 1
}

# quota_left USED LIMIT: the space left, or "none".
quota_left() {
  [ "$2" -gt "$1" ] && format_size $(( $2 - $1 )) || echo none
}
END_FILE
cat >lib/size.sh <<'END_FILE'
# size.sh: human-readable byte sizes.
#
# format_size BYTES prints BYTES in powers of 1024 with a one-letter unit (B, K, M,
# G, T, P): 512B, 1.5K, 2.0M. fmt_size_si BYTES does the same in powers of
# 1000 with SI units (B, kB, MB, GB, TB, PB). Both print an error and return
# 1 when BYTES is not a whole number.

format_size() {
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

# fmt_size_list BYTES...: one format_size result per line.
fmt_size_list() {
  for b in "$@"; do
    format_size "$b" || return 1
  done
}

# size_with BYTES UNITS: format_size for UNITS iec, fmt_size_si for UNITS si.
size_with() {
  case $2 in
    si) fmt=fmt_size_si ;;
    *) fmt=format_size ;;
  esac
  "$fmt" "$1"
}
END_FILE
cat >lib/units.sh <<'END_FILE'
# units.sh: whole-number unit conversions. For display, use format_size
# (lib/size.sh) instead.

kib() { echo $(( $1 * 1024 )); }
mib() { echo $(( $1 * 1024 * 1024 )); }
gib() { echo $(( $1 * 1024 * 1024 * 1024 )); }
kb() { echo $(( $1 * 1000 )); }
mb() { echo $(( $1 * 1000 * 1000 )); }
END_FILE
cat >lib/util.sh <<'END_FILE'
# util.sh: small helpers shared by the commands.

die() {
  echo "dus: $*" >&2
  exit 1
}

is_uint() {
  case $1 in
    ''|*[!0-9]*) return 1 ;;
  esac
  return 0
}

# need_sizes: load size.sh unless format_size is already defined.
need_sizes() {
  command -v format_size >/dev/null 2>&1 || . "$DUS_ROOT/lib/size.sh"
}
END_FILE
cat >lib/fmt/bar.sh <<'END_FILE'
# bar.sh: text bar charts.

# bar WIDTH VALUE MAX: a bar of # characters, VALUE/MAX of WIDTH.
bar() {
  n=$(( $1 * $2 / $3 ))
  out=
  while [ "$n" -gt 0 ]; do out="$out#"; n=$(( n - 1 )); done
  printf '%s\n' "$out"
}

# bar_chart FILE: one bar per scan line, scaled to the largest file.
bar_chart() {
  max=$(scan_largest "$1" | cut -d ' ' -f 2)
  format_size $max | sed 's/^/max: /'
  grep -v '^#' "$1" | while read -r path bytes; do
    printf '%-20s %s\n' "$path" "$(bar 30 "$bytes" "$max")"
  done
}
END_FILE
cat >lib/fmt/json.sh <<'END_FILE'
# json.sh: JSON output.

# json_size NAME BYTES: {"name": NAME, "bytes": BYTES, "human": SIZE}, with
# SIZE from format_size.
json_size() {
  fmt_size=$(format_size "$2") || return 1
  printf '{"name": "%s", "bytes": %s, "human": "%s"}\n' "$1" "$2" "$fmt_size"
}

json_lines() {
  grep -v '^#' "$1" | while read -r path bytes; do
    json_size "$path" "$bytes"
  done
}
END_FILE
cat >lib/fmt/legacy.sh <<'END_FILE'
# legacy.sh: names kept for scripts written against dus 0.3.

# fmtsize BYTES: the old name of format_size.
fmtsize() {
  format_size "$@"
}

# fmtsize_si BYTES: the old name of fmt_size_si.
fmtsize_si() {
  fmt_size_si "$@"
}
END_FILE
cat >lib/fmt/table.sh <<'END_FILE'
# table.sh: fixed-width rows.

# table_row NAME BYTES: NAME padded to 24 columns, then the size
# right-aligned in 8.
table_row() {
  printf '%-24s %8s\n' "$1" "$(format_size "$2")"
}

# table_rows: table_row for each "NAME BYTES" line on stdin.
table_rows() {
  while read -r name bytes; do
    s=`format_size $bytes`; printf '%-24s %8s\n' "$name" "$s"
  done
}
END_FILE
cat >lib/report/csv.sh <<'END_FILE'
# csv.sh: CSV output, one row per file: path,bytes,human.

csv_rows() {
  echo "path,bytes,human"
  grep -v '^#' "$1" | while read -r path bytes; do
    printf '%s,%s,' "$path" "$bytes"; format_size "$bytes"
  done
}
END_FILE
cat >lib/report/diff.sh <<'END_FILE'
# diff.sh: compare two scans.

# size_delta NEW OLD: "+SIZE" or "-SIZE", the change as format_size prints it.
size_delta() {
  if [ "$1" -ge "$2" ]; then
    echo "+$(format_size $(( $1 - $2 )))"
  else
    echo "-$(format_size $(( $2 - $1 )))"
  fi
}

# scan_diff NEW OLD: the change in total size between two scan files.
scan_diff() {
  size_delta "$(scan_total "$1")" "$(scan_total "$2")"
}
END_FILE
cat >lib/report/summary.sh <<'END_FILE'
# summary.sh: the summary report: file count, total and average size.

summary() {
  total=$(scan_total "$1")
  count=$(scan_count "$1")
  echo "files: $count"
  echo "total: $(format_size "$total")"
  if [ "$count" -gt 0 ]; then
    echo "average: $(format_size $(( total / count )))"
  fi
}
END_FILE
cat >lib/report/top.sh <<'END_FILE'
# top.sh: the largest files first.

# top_n FILE N: the N largest files, each as "SIZE PATH".
top_n() {
  grep -v '^#' "$1" | sort -k 2,2nr -k 1,1 | head -n "$2" | while read -r path bytes; do
format_size "$bytes" | tr '\n' ' '; echo "$path"
  done
}
END_FILE
cat >tests/run.sh <<'END_FILE'
#!/bin/sh
# Run from the project root: sh tests/run.sh
DUS_ROOT=$(pwd)
export DUS_ROOT
for lib in util size parse scan quota units fmt/table fmt/json fmt/legacy \
  report/summary report/top report/diff report/csv; do
  . "lib/$lib.sh"
done

fail=0
check() {
  if [ "$2" != "$3" ]; then
    printf 'FAIL %s\n  got:  %s\n  want: %s\n' "$1" "$2" "$3"
    fail=1
  fi
}

while read -r bytes iec si; do
  case $bytes in '#'*) continue ;; esac
  check "fmt_size $bytes" "$(format_size "$bytes")" "$iec"
  check "fmt_size_si $bytes" "$(fmt_size_si "$bytes")" "$si"
done <tests/fixtures/sizes.txt

check "fmt_size bad" "$(format_size 12x 2>&1)" "fmt_size: not a number: 12x"
check "fmt_size_si bad" "$(fmt_size_si '' 2>&1)" "fmt_size_si: not a number: "
check "fmt_size_list" "$(fmt_size_list 1 2048)" "$(printf '1B\n2.0K')"
check "size_with si" "$(size_with 2000000 si)" "2.0MB"
check "size_with iec" "$(size_with 2097152 iec)" "2.0M"
check "fmtsize" "$(fmtsize 3072)" "3.0K"
check "parse_size" "$(parse_size 1.5K)" "1536"
check "round_trip" "$(round_trip 1600)" "1638"
check "quota ok" "$(quota_check 10 20)" "ok"
check "quota over" "$(quota_check 3072 1024)" "over by 2.0K"
check "quota left" "$(quota_left 1024 3072)" "2.0K"
check "quota none" "$(quota_left 3072 1024)" "none"
check "kib" "$(kib 3)" "3072"
check "table_row" "$(table_row docs 1536)" "docs                         1.5K"
check "json" "$(json_size a 2048)" '{"name": "a", "bytes": 2048, "human": "2.0K"}'
check "summary" "$(summary tests/fixtures/scan.txt)" "$(cat tests/fixtures/summary.txt)"
check "top" "$(top_n tests/fixtures/scan.txt 2)" "$(printf '3.0M src/big.bin\n1.0M img/logo.png')"
check "diff" "$(scan_diff tests/fixtures/scan.txt tests/fixtures/old-scan.txt)" "+1.0M"
check "csv" "$(csv_rows tests/fixtures/scan.txt | sed -n 3p)" "docs/b.txt,1536,1.5K"
check "sizeof" "$(sh bin/sizeof.sh -s 1500 2>&1)" "1.5kB"
check "sizeof usage" "$(sh bin/sizeof.sh 2>&1 | sed -n 2p)" \
  "Prints each BYTES as fmt_size does (powers of 1024), or with -s as"
check "dus largest" "$(sh bin/dus.sh largest tests/fixtures/scan.txt)" "src/big.bin 3.0M"
check "dus hist" "$(sh bin/dus.sh hist tests/fixtures/scan.txt | tail -n 1)" "  <=4M 1"
check "topn" "$(sh bin/topn.sh 2 tests/fixtures/scan.txt)" "$(printf '3.0M\n1.0M')"
check "quota.sh" "$(sh bin/quota.sh 1024 3072 | sed -n 3p)" "left:   2.0K"
check "compare" "$(sh bin/compare.sh tests/fixtures/scan.txt tests/fixtures/old-scan.txt | tail -n 1)" \
  "largest in tests/fixtures/old-scan.txt: src/big.bin (2.0M)"
check "explain" "$(sh contrib/explain.sh 2048 | head -n 1)" "fmt_size(2048) = 2.0K"

if [ $fail -eq 0 ]; then echo "all tests passed"; fi
exit $fail
END_FILE
