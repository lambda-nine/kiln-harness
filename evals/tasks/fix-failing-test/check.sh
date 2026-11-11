cd /workspace || exit 2
[ -f test.sh ] && [ -f lib.sh ] || exit 1
[ "$(sha256sum test.sh | cut -d ' ' -f 1)" = "b97b8d5157d700345ae6071be9bb7fc520ca8d5ccbbff07541165be434cac02a" ] || exit 1
sh test.sh </dev/null >/dev/null 2>&1 || exit 1
