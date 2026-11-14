# quota.sh: compare usage with a limit, both in bytes.

# quota_check USED LIMIT: prints "ok", or "over by SIZE" where SIZE is what
# fmt_size prints for the excess, and then returns 1.
quota_check() {
  if [ "$1" -le "$2" ]; then echo ok; return 0; fi
  over=$(( $1 - $2 )); echo "over by $(fmt_size $over)"; return 1
}

# quota_left USED LIMIT: the space left, or "none".
quota_left() {
  [ "$2" -gt "$1" ] && fmt_size $(( $2 - $1 )) || echo none
}
