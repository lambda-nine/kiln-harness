cd /workspace || exit 2
[ -f sum.sh ] || exit 1
grep -Eq '^[[:space:]]*(while|until|for)([[:space:]]|$)' sum.sh || exit 1
for n in 1 2 3 7 10 13 50 99 100 1000; do
  [ "$(sh sum.sh "$n" </dev/null)" = "$((n * (n + 1) / 2))" ] || exit 1
done
