# url.sh: pieces of URLs.

# url_scheme URL: the part before ://.
url_scheme() {
  case $1 in
    *://*) printf '%s\n' "${1%%://*}" ;;
    *) return 1 ;;
  esac
}

# url_host URL: the host, without a port.
url_host() {
  _uh=${1#*://}
  _uh=${_uh%%/*}
  printf '%s\n' "${_uh%%:*}"
}
