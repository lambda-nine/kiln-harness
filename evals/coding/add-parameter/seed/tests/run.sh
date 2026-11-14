#!/bin/sh
# Run from the project root: sh tests/run.sh
ROOT=$(pwd)
export ROOT
. lib/log.sh
. lib/lock.sh
. lib/fs/prune.sh

fail=0
check() {
  if [ "$2" != "$3" ]; then
    printf 'FAIL %s\n  got:  %s\n  want: %s\n' "$1" "$2" "$3"
    fail=1
  fi
}

tmp=$(mktemp -d) || exit 1
trap 'rm -rf "$tmp"' EXIT

check "info" "$(log_event info hello 2>&1)" "info: hello"
check "debug hidden" "$(log_event debug hidden 2>&1)" ""
check "debug shown" "$(LOG_LEVEL=debug log_event debug shown 2>&1)" "debug: shown"
check "custom level" "$(LOG_LEVEL=error log_event notice always 2>&1)" "notice: always"
LOG_FILE=$tmp/log
log_event warn "to the file" 2>/dev/null
unset LOG_FILE
check "log file" "$(cat "$tmp/log")" "warn: to the file"
printf 'info: a\nwarn: b\ninfo [x]: c\nerror: d\n' >"$tmp/counts"
check "log_events" "$(log_events "$tmp/counts" | tr '\n' ' ')" "debug 0 info 2 warn 1 error 1 "
check "lock" "$(lock_take "$tmp/lock" 2>&1 && echo ok)" "ok"
check "lock held" "$(lock_take "$tmp/lock" 2>/dev/null || echo held)" "held"
check "prune none" "$(prune_old "$tmp/none" 3 2>/dev/null)" "0"
check "rotate missing" "$(sh bin/rotate.sh "$tmp/nope" 2>&1)" "warn [rotate]: nothing to rotate: $tmp/nope"
check "nightly dry" "$(sh bin/nightly -n 2>/dev/null | head -n 1)" "would run backup"

if [ $fail -eq 0 ]; then echo "all tests passed"; fi
exit $fail
