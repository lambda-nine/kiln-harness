cd /workspace || exit 2
[ -f sum.sh ] || exit 1
[ "$(sh sum.sh 1 </dev/null)" = 1 ] || exit 1
[ "$(sh sum.sh 10 </dev/null)" = 55 ] || exit 1
[ "$(sh sum.sh 100 </dev/null)" = 5050 ] || exit 1
