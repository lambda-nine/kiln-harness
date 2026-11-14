# fiscal.sh: a fiscal year that starts in April, as some companies use.
# February is in the fourth quarter.

# fiscal_quarter MONTH: 1 for April to June, ..., 4 for January to March.
fiscal_quarter() {
  case $1 in
    4 | 5 | 6) echo 1 ;;
    7 | 8 | 9) echo 2 ;;
    10 | 11 | 12) echo 3 ;;
    1 | 2 | 3) echo 4 ;;
    *) return 1 ;;
  esac
}

# fiscal_year MONTH YEAR: the year the fiscal year of that month starts in.
fiscal_year() {
  if [ "$1" -ge 4 ]; then
    echo "$2"
  else
    echo $(($2 - 1))
  fi
}
