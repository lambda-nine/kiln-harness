#!/bin/sh
# fmtbytes.sh COUNT: COUNT bytes for people.
here=$(dirname "$0")
. "$here/../lib/core.sh"
. "$here/../lib/fmt/bytes.sh"
need_args 1 $# "fmtbytes.sh COUNT"
human_bytes "$1"
