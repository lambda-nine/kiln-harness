#!/bin/sh
# daysin.sh MONTH YEAR: how many days MONTH has in YEAR.
here=$(dirname "$0")
. "$here/../lib/core.sh"
. "$here/../lib/calendar.sh"
. "$here/../lib/dates.sh"
need_args 2 $# "daysin.sh MONTH YEAR"
is_int "$1" && is_int "$2" || die "MONTH and YEAR must be numbers"
days_in_month "$1" "$2" || die "no such month: $1"
