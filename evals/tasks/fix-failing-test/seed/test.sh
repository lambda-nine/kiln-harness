#!/bin/sh
# Run from /workspace: sh test.sh
. ./lib.sh
fail=0
check() {
  got=$(slugify "$1")
  if [ "$got" != "$2" ]; then
    echo "FAIL: slugify '$1' gave '$got', want '$2'"
    fail=1
  fi
}
check "Hello World" "hello-world"
check "  Kiln: Sandboxes & Harnesses!  " "kiln-sandboxes-harnesses"
check "already-a-slug" "already-a-slug"
check "Version 2.0 -- Released" "version-2-0-released"
check "ABC123" "abc123"
if [ "$fail" -eq 0 ]; then
  echo "all tests passed"
fi
exit "$fail"
