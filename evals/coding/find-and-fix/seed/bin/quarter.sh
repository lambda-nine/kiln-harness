#!/bin/sh
# quarter.sh MONTH YEAR: the fiscal quarter and year of a month.
here=$(dirname "$0")
. "$here/../lib/core.sh"
. "$here/../lib/fiscal.sh"
need_args 2 $# "quarter.sh MONTH YEAR"
printf 'Q%s FY%s\n' "$(fiscal_quarter "$1")" "$(fiscal_year "$1" "$2")"
