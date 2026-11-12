# banner.sh: the start-up banner, from config_get name.
banner() {
  printf '== %s ==\n' "$(config_get name)"
}
