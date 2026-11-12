#!/bin/sh
# app.sh: print the banner, the address and the service name.
cd "$(dirname "$0")/.." || exit 1
. lib/config.sh
. lib/log.sh
. lib/net.sh
. lib/util.sh
. lib/sub/banner.sh

banner
log_msg "listening on $(net_addr), config $(get_cfg_path)"
get_cfg name
