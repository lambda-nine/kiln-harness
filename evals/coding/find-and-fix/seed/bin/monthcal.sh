#!/bin/sh
# monthcal.sh YEAR: each month's name and length in YEAR.
here=$(dirname "$0")
. "$here/../lib/core.sh"
. "$here/../lib/calendar.sh"
. "$here/../lib/dates.sh"
need_args 1 $# "monthcal.sh YEAR"
m=1
while [ "$m" -le 12 ]; do
  printf '%-9s %s\n' "$(month_name "$m")" "$(days_in_month "$m" "$1")"
  m=$((m + 1))
done
