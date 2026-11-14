# units.sh: whole-number unit conversions. For display, use fmt_size
# (lib/size.sh) instead.

kib() { echo $(( $1 * 1024 )); }
mib() { echo $(( $1 * 1024 * 1024 )); }
gib() { echo $(( $1 * 1024 * 1024 * 1024 )); }
kb() { echo $(( $1 * 1000 )); }
mb() { echo $(( $1 * 1000 * 1000 )); }
