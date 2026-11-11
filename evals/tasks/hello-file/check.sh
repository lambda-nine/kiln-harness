cd /workspace || exit 2
[ -f hello.txt ] || exit 1
printf 'hello, kiln\n' | cmp -s - hello.txt || exit 1
