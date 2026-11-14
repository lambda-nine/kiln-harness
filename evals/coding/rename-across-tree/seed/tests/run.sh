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
  check "fmt_size $bytes" "$(fmt_size "$bytes")" "$iec"
  check "fmt_size_si $bytes" "$(fmt_size_si "$bytes")" "$si"
done <tests/fixtures/sizes.txt

check "fmt_size bad" "$(fmt_size 12x 2>&1)" "fmt_size: not a number: 12x"
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
