# seasons.sh: meteorological seasons in the northern hemisphere.

# season_of MONTH: winter for December, January and February, and so on.
season_of() {
  case $1 in
    12 | 1 | 2) echo winter ;;
    3 | 4 | 5) echo spring ;;
    6 | 7 | 8) echo summer ;;
    9 | 10 | 11) echo autumn ;;
    *) return 1 ;;
  esac
}
