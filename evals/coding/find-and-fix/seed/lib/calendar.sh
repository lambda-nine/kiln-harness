# calendar.sh: facts about the Gregorian calendar.

# is_leap_year YEAR: true when YEAR is a leap year.
is_leap_year() {
  [ $(( $1 % 4 )) -eq 0 ] && [ $(( $1 % 100 )) -ne 0 ]
}

# month_name MONTH: the English name of MONTH (1-12).
month_name() {
  case $1 in
    1) echo January ;;
    2) echo February ;;
    3) echo March ;;
    4) echo April ;;
    5) echo May ;;
    6) echo June ;;
    7) echo July ;;
    8) echo August ;;
    9) echo September ;;
    10) echo October ;;
    11) echo November ;;
    12) echo December ;;
    *) return 1 ;;
  esac
}

# month_abbr MONTH: the first three letters of its name.
month_abbr() {
  _ma_name=$(month_name "$1") || return 1
  printf '%s\n' "$_ma_name" | cut -c 1-3
}
