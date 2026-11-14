# week.sh: days of the week, 1 for Monday.

# weekday_name DAY
weekday_name() {
  case $1 in
    1) echo Monday ;;
    2) echo Tuesday ;;
    3) echo Wednesday ;;
    4) echo Thursday ;;
    5) echo Friday ;;
    6) echo Saturday ;;
    7) echo Sunday ;;
    *) return 1 ;;
  esac
}

# is_weekend DAY
is_weekend() {
  [ "$1" -ge 6 ] && [ "$1" -le 7 ]
}
