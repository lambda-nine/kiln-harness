#!/bin/sh
# textkit.sh COMMAND ARG...: run one textkit helper by name.
here=$(dirname "$0")
for module in core strings numbers lists paths calendar dates; do
  . "$here/../lib/$module.sh"
done
[ $# -ge 1 ] || die "usage: textkit.sh COMMAND ARG..."
"$@"
