#!/bin/sh
# Run from the project root: sh tests/run.sh
. lib/config.sh
. lib/log.sh
. lib/net.sh
. lib/util.sh
. lib/sub/banner.sh

fail=0
expect() {
  if [ "$2" = "$3" ]; then
    echo "ok $1"
  else
    echo "FAIL $1: got [$2], want [$3]"
    fail=1
  fi
}

expect port "$(config_get port)" 8080
expect missing "$(config_get nope)" ""
expect path "$(config_get_path)" etc/app.conf
expect count "$(cfg_count)" 4
expect addr "$(net_addr)" localhost:8080
expect retries "$(retries)" 3
expect require "$(require host)" localhost
expect banner "$(banner)" "== demo =="
expect override "$(my_config_get retries)" 3
expect prefix "$CFG_PREFIX" config_get_
expect accessor "$(accessor_name port)" config_get_port
expect log "$(log_msg hi)" "[info] hi"
expect app "$(sh bin/app.sh | tail -n 1)" demo
exit $fail
