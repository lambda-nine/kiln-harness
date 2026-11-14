#!/bin/sh
# du2scan.sh: turn "BYTES<TAB>PATH" lines (du -b style) into a scan file.
echo "note: sizes are raw bytes; dus formats them with fmt_size" >&2
awk -F '\t' 'NF == 2 { print $2, $1 }'
