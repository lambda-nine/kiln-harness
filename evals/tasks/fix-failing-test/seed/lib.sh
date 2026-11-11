# slugify TEXT: print TEXT in lower case, with each run of characters other
# than a-z and 0-9 replaced by a single hyphen, and no leading or trailing
# hyphen.
slugify() {
  printf '%s\n' "$1" | tr 'A-Z' 'a-z' | sed -e 's/[^a-z0-9]/-/g'
}
