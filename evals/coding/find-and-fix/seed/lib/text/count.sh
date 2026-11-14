# count.sh: counting text.

# count_words: words on standard input.
count_words() {
  wc -w | tr -d ' '
}

# count_lines: lines on standard input.
count_lines() {
  wc -l | tr -d ' '
}
