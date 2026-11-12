cd /workspace || exit 2
LC_ALL=C
export LC_ALL
rm -rf /workspace/.check && mkdir /workspace/.check || exit 2
C=/workspace/.check
[ -f order.sh ] || exit 1
[ "$(sha256sum deps.txt | cut -d ' ' -f 1)" = "8abf9a41d0179f03b8a746eba043b8d3a878005efb92eb84ed1c659a1e0e5761" ] || exit 1
n=0
# run STATUS FILE: stdout must equal $C/want and the exit status STATUS;
# for a non-zero status, stdout must be empty and stderr equal $C/err.
run() {
  n=$((n + 1))
  sh /workspace/order.sh "$2" </dev/null >"$C/out" 2>"$C/got-err"
  rc=$?
  [ "$1" = 0 ] || : >"$C/want"
  if [ "$rc" != "$1" ] || ! cmp -s "$C/out" "$C/want"; then
    echo "case $n"
    exit 1
  fi
  if [ "$1" != 0 ] && ! cmp -s "$C/got-err" "$C/err"; then
    echo "case $n"
    exit 1
  fi
}

# The prompt's example.
cat >"$C/want" <<'END'
config.h
fixtures
icons
libc
lib-gfx
lib-net
lib-ui
app
test
END
run 0 /workspace/deps.txt

# The smallest ready name, not a depth-first post-order.
cat >"$C/g1" <<'END'
app: z_lib a_lib
z_lib: core
a_lib: util
END
printf 'core\nutil\na_lib\nz_lib\napp\n' >"$C/want"
run 0 "$C/g1"

# Byte order: punctuation, digits, upper case, underscore, lower case.
printf 'all: b B a A _x 10 9 -x 1.5\n' >"$C/g2"
printf '%s\n' -x 1.5 10 9 A B _x a b all >"$C/want"
run 0 "$C/g2"

# A target on several lines, and names that appear only as dependencies.
cat >"$C/g3" <<'END'
web: api
api: db
web: cache
api: auth
END
printf 'auth\ncache\ndb\napi\nweb\n' >"$C/want"
run 0 "$C/g3"

# Comments and blank lines with leading blanks, tabs everywhere, repeated
# dependencies, a target without dependencies, and no LF at the end.
printf '  # setup\n\t# more\n \t \nz:\n\tm\t:\tk \t a\n\nk: a a  \n  y :z\n# x: q\nq' >"$C/g4"
printf 'syntax error on line 10\n' >"$C/err"
run 2 "$C/g4"
printf '  # setup\n\t# more\n \t \nz:\n\tm\t:\tk \t a\n\nk: a a  \n  y :z\n# x: q\nq:' >"$C/g5"
printf 'a\nk\nm\nq\nz\ny\n' >"$C/want"
run 0 "$C/g5"

# A diamond with several equally ready names at each step.
cat >"$C/g6" <<'END'
d: b c
b: a
c: a
e: d a
f: a
END
printf 'a\nb\nc\nd\ne\nf\n' >"$C/want"
run 0 "$C/g6"

# A cycle three deep behind valid nodes, and a self-dependency.
cat >"$C/c1" <<'END'
ok1: ok2
ok2:
x: y
y: z
z: x
w: x ok1
END
printf 'cycle\n' >"$C/err"
run 3 "$C/c1"
printf 'a: b\nb: b\nc: a\n' >"$C/c2"
run 3 "$C/c2"
printf 'solo: solo\n' >"$C/c3"
run 3 "$C/c3"

# Syntax errors, numbered by line, the first one reported, and checked
# before cycles.
printf 'a: b\nfoo bar: baz\n' >"$C/s1"
printf 'syntax error on line 2\n' >"$C/err"
run 2 "$C/s1"
printf '# c\n\na: b # trailing comment\n' >"$C/s2"
printf 'syntax error on line 3\n' >"$C/err"
run 2 "$C/s2"
printf 'a: b\n\n: c\nd e\n' >"$C/s3"
printf 'syntax error on line 3\n' >"$C/err"
run 2 "$C/s3"
printf 'a b\n' >"$C/s4"
printf 'syntax error on line 1\n' >"$C/err"
run 2 "$C/s4"
printf 'a: b\n x: y: z\n' >"$C/s5"
printf 'syntax error on line 2\n' >"$C/err"
run 2 "$C/s5"
printf 'a: a\nb: c/d\n' >"$C/s6"
printf 'syntax error on line 2\n' >"$C/err"
run 2 "$C/s6"
printf 'a: b\r\n' >"$C/s7"
printf 'syntax error on line 1\n' >"$C/err"
run 2 "$C/s7"

# Empty, and only comments and blank lines.
: >"$C/e1"
: >"$C/want"
run 0 "$C/e1"
printf '# nothing\n\n   \n' >"$C/e2"
run 0 "$C/e2"
