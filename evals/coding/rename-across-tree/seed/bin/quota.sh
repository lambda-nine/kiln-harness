#!/bin/sh
# quota.sh USED LIMIT: usage against a limit, both in bytes.
DUS_ROOT=${DUS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
. "$DUS_ROOT/lib/util.sh"
. "$DUS_ROOT/lib/size.sh"
. "$DUS_ROOT/lib/quota.sh"

is_uint "$1" && is_uint "$2" || die "usage: quota.sh USED LIMIT"
cat <<EOF
used:   $(fmt_size "$1")
limit:  $(fmt_size "$2")
left:   $(quota_left "$1" "$2")
status: $(quota_check "$1" "$2")
EOF
