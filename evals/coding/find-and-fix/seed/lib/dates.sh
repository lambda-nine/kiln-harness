# dates.sh: date arithmetic. Source calendar.sh first.

# days_in_month MONTH YEAR: how many days MONTH (1-12) has in YEAR.
days_in_month() {
  case $1 in
    1 | 3 | 5 | 7 | 8 | 10 | 12) echo 31 ;;
    4 | 6 | 9 | 11) echo 30 ;;
    2)
      if is_leap_year "$2"; then
        echo 29
      else
        echo 28
      fi
      ;;
    *) return 1 ;;
  esac
}

# day_of_year DAY MONTH YEAR: 1 for the first of January.
day_of_year() {
  _doy=$1
  _doy_m=1
  while [ "$_doy_m" -lt "$2" ]; do
    _doy=$((_doy + $(days_in_month "$_doy_m" "$3")))
    _doy_m=$((_doy_m + 1))
  done
  echo "$_doy"
}

# valid_date DAY MONTH YEAR: true when the date exists.
valid_date() {
  [ "$2" -ge 1 ] && [ "$2" -le 12 ] || return 1
  [ "$1" -ge 1 ] && [ "$1" -le "$(days_in_month "$2" "$3")" ]
}
