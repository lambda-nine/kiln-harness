# paths.sh: path name helpers. They never touch the file system.

# path_base PATH: the last component.
path_base() {
  printf '%s\n' "${1##*/}"
}

# path_dir PATH: everything before the last component, or ".".
path_dir() {
  case $1 in
    */*) printf '%s\n' "${1%/*}" ;;
    *) echo . ;;
  esac
}

# path_ext PATH: the last component's extension, without the dot.
path_ext() {
  _pe_base=${1##*/}
  case $_pe_base in
    ?*.*) printf '%s\n' "${_pe_base##*.}" ;;
    *) echo '' ;;
  esac
}
