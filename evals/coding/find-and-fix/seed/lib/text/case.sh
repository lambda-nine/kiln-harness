# case.sh: changing the case of words.

# title_case TEXT: each word with its first letter upper case.
title_case() {
  printf '%s\n' "$1" | awk '{
    for (i = 1; i <= NF; i++) $i = toupper(substr($i, 1, 1)) substr($i, 2)
    print
  }'
}

# snake_to_kebab TEXT
snake_to_kebab() {
  printf '%s\n' "$1" | tr _ -
}
