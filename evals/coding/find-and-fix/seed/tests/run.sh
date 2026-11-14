#!/bin/sh
# The textkit tests. Run from the top directory: sh tests/run.sh
cd "$(dirname "$0")/.." || exit 2
. ./lib/core.sh
. ./lib/strings.sh
. ./lib/numbers.sh
. ./lib/lists.sh
. ./lib/paths.sh
. ./lib/calendar.sh
. ./lib/dates.sh
. ./lib/fmt/number.sh
. ./lib/fmt/duration.sh

failures=0

# check NAME WANT GOT
check() {
  if [ "$2" = "$3" ]; then
    printf 'ok   %s\n' "$1"
  else
    printf 'FAIL %s: want %s, got %s\n' "$1" "$2" "$3"
    failures=$((failures + 1))
  fi
}

check "trim" "a b" "$(trim '  a b  ')"
check "to_upper" "ABC" "$(to_upper abc)"
check "repeat_str" "ababab" "$(repeat_str ab 3)"
check "clamp below" "1" "$(clamp -5 1 9)"
check "clamp above" "9" "$(clamp 12 1 9)"
check "sum_list" "10" "$(sum_list 1 2 3 4)"
check "join_by" "a-b-c" "$(join_by - a b c)"
check "path_ext" "gz" "$(path_ext /tmp/a.tar.gz)"
check "with_commas" "1,234,567" "$(with_commas 1234567)"
check "human_duration" "1h 2m 3s" "$(human_duration 3723)"
check "month_name" "February" "$(month_name 2)"
check "January 2023" "31" "$(days_in_month 1 2023)"
check "April 2023" "30" "$(days_in_month 4 2023)"
check "February 2023" "28" "$(days_in_month 2 2023)"
check "February 2024" "29" "$(days_in_month 2 2024)"
check "February 1900" "28" "$(days_in_month 2 1900)"
check "February 2000" "29" "$(days_in_month 2 2000)"
check "day_of_year" "60" "$(day_of_year 29 2 2000)"
check "valid_date" "yes" "$(valid_date 29 2 2000 && echo yes || echo no)"

if [ "$failures" -ne 0 ]; then
  printf '%d failed\n' "$failures"
  exit 1
fi
echo "all passed"
