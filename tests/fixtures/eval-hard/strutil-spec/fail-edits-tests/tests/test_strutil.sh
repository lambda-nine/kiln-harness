#!/bin/sh
# Tests for lib/strutil.sh. Run from the project root: sh tests/test_strutil.sh
. lib/strutil.sh

failures=0
tab=$(printf '\t')

# expect NAME WANT GOT
expect() {
  if [ "$2" = "$3" ]; then
    echo "ok $1"
  else
    echo "FAIL $1: want [$2], got [$3]"
    failures=$((failures + 1))
  fi
}

expect trim-spaces "a b" "$(str_trim '  a b  ')"
expect trim-tabs "x" "$(str_trim "${tab}x $tab")"
expect pad-default "   42" "$(str_pad_left 42 5)"
expect pad-char "00042" "$(str_pad_left 42 5 0)"
expect pad-long "abcdef" "$(str_pad_left abcdef 3)"
expect repeat-three "ababab" "$(str_repeat ab 3)"
expect repeat-zero "" "$(str_repeat ab 0)"
expect count-simple "2" "$(str_count 'a,b,c' ,)"
expect count-nonoverlapping "2" "$(str_count aaaa aa)"
expect lower "hello, world" "$(str_lower 'Hello, WORLD')"
str_starts_with foobar foo
expect starts-yes 0 "$?"
str_starts_with foobar bar
expect starts-no 1 "$?"

if [ "$failures" -ne 0 ]; then
  echo "$failures failed"
  exit 1
fi
echo "all passed"
