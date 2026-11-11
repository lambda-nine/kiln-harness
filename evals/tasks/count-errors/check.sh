cd /workspace || exit 2
[ -f answer.txt ] || exit 1
printf '6\n' | cmp -s - answer.txt || exit 1
