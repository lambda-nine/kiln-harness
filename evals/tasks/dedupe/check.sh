cd /workspace || exit 2
[ -f list.txt ] || exit 1
cmp -s - list.txt <<'END' || exit 1
pear
apple
Apple
banana
cherry
banana split
fig
Fig
END
