# paths.sh: path helpers.

path_base() {
  printf '%s\n' "${1##*/}"
}

path_dir() {
  case $1 in
    */*) printf '%s\n' "${1%/*}" ;;
    *) echo . ;;
  esac
}
