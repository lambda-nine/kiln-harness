cd /workspace || exit 2
LC_ALL=C
export LC_ALL
rm -rf /workspace/.check && mkdir /workspace/.check || exit 2
C=/workspace/.check
[ -f stak.sh ] || exit 1
[ "$(find docs examples | sort | tr '\n' ' ')" = "docs docs/stak.md examples examples/basics.out examples/basics.stk examples/countdown.out examples/countdown.stk examples/loops.out examples/loops.stk examples/words.out examples/words.stk " ] || exit 1
[ "$(for f in $(find docs examples -type f | sort); do sha256sum "$f" | cut -d ' ' -f 1; done | sha256sum | cut -d ' ' -f 1)" = "2ae1bf1138670d600ebd1ff28b87453509b9412965923b96a968f985e9fac16d" ] || exit 1
n=0

# go FILE: run FILE from / with a 10 second limit; sets rc.
go() {
  rm -f "$C/timeout"
  (cd / && exec sh /workspace/stak.sh "$1") </dev/null >"$C/out" 2>"$C/err" &
  pid=$!
  (
    s=0
    while [ "$s" -lt 10 ]; do
      sleep 1
      kill -0 "$pid" 2>/dev/null || exit 0
      s=$((s + 1))
    done
    : >"$C/timeout"
    kill -9 "$pid"
  ) </dev/null >/dev/null 2>&1 &
  wd=$!
  wait "$pid" 2>/dev/null
  rc=$?
  kill "$wd" 2>/dev/null
  wait "$wd" 2>/dev/null
  if [ -f "$C/timeout" ]; then
    echo "case $n: over 10 s"
    exit 1
  fi
}

# t STATUS OUT [ERR]: run $C/p.stk; stdout must be the printf format OUT,
# stderr the single line ERR (or empty) and the exit status STATUS.
t() {
  n=$((n + 1))
  printf '%b' "$2" >"$C/want"
  if [ $# -ge 3 ]; then printf '%s\n' "$3" >"$C/werr"; else : >"$C/werr"; fi
  go "$C/p.stk"
  if [ "$rc" != "$1" ] || ! cmp -s "$C/out" "$C/want" || ! cmp -s "$C/err" "$C/werr"; then
    echo "case $n"
    exit 1
  fi
}

# syn N: $C/p.stk has a syntax error at line N.
syn() {
  t 2 '' "error: syntax at line $1"
}

# The examples.
for e in basics countdown loops words; do
  n=$((n + 1))
  go "/workspace/examples/$e.stk"
  if [ "$rc" != 0 ] || ! cmp -s "$C/out" "examples/$e.out" || [ -s "$C/err" ]; then
    echo "case $n"
    exit 1
  fi
done

# Recursion, including deep recursion.
cat >"$C/p.stk" <<'END'
: fact dup 1 > if dup 1 - fact * then ;
1 fact . 5 fact . 12 fact . cr
: sum dup if dup 1 - sum + then ;
1000 sum . cr
END
t 0 '1 120 479001600 \n500500 \n'

# FizzBuzz: nested if/else inside begin/while/repeat inside a definition.
cat >"$C/p.stk" <<'END'
: fizzbuzz
  1 begin dup 16 < while
    dup 15 mod 0 = if ." FizzBuzz" 32 emit else
      dup 3 mod 0 = if ." Fizz" 32 emit else
        dup 5 mod 0 = if ." Buzz" 32 emit else dup . then
      then
    then
    1 +
  repeat drop ;
fizzbuzz cr
END
t 0 '1 2 Fizz 4 Buzz Fizz 7 8 Fizz Buzz 11 Fizz 13 14 FizzBuzz \n'

# begin/until nested in begin/until at the top level.
cat >"$C/p.stk" <<'END'
3 begin
  dup 0 begin 1 + dup . over over = until drop drop
  ." /" 32 emit
  1 - dup not
until drop cr
END
t 0 '1 2 3 / 1 2 / 1 / \n'

# Division truncates toward zero; mod takes the dividend's sign.
cat >"$C/p.stk" <<'END'
-17 5 / . -17 5 mod . 17 -5 / . 17 -5 mod . -17 -5 / . -17 -5 mod . 0 -3 / . -1 2 / . cr
END
t 0 '-3 -2 -3 2 3 -2 0 0 \n'

# A body keeps the definition current when it was read.
cat >"$C/p.stk" <<'END'
: greet ." hello" 32 emit ;
: twice greet greet ;
: greet ." bye" 32 emit ;
twice greet cr
END
t 0 'hello hello bye \n'

# Self-recursion binds to the new definition, not the older one.
cat >"$C/p.stk" <<'END'
: down ." old" cr ;
: down dup . dup if 1 - down else drop then ;
3 down cr
END
t 0 '3 2 1 0 \n'

# A body's call to a word defined only later is unknown when it runs.
cat >"$C/p.stk" <<'END'
: main
  helper ;
: helper ." helped" cr ;
." start" cr
main
END
t 3 'start\n' 'error: unknown word helper at line 2'

# So is a top-level call before the definition.
cat >"$C/p.stk" <<'END'
1 . later
: later 2 . ;
END
t 3 '1 ' 'error: unknown word later at line 1'

# An unknown word that never runs is not an error; names are case-sensitive.
cat >"$C/p.stk" <<'END'
: safe 0 if Missing then 4 . ;
: DUP 5 . ;
safe DUP cr
END
t 0 '4 5 \n'

# A runtime error inside a body names the body's line, after partial output.
cat >"$C/p.stk" <<'END'
: show
  . . ;
1 2 show cr
7 show
END
t 3 '2 1 \n7 ' 'error: stack underflow at line 2'

cat >"$C/p.stk" <<'END'
10 3 mod .
5 0 mod .
END
t 3 '1 ' 'error: division by zero at line 2'

cat >"$C/p.stk" <<'END'
72 emit 105 emit 33 emit 32 emit 126 emit 10 emit
END
t 3 'Hi! ~' 'error: bad emit at line 1'

# Strings keep #; a glued # is part of a word.
cat >"$C/p.stk" <<'END'
." a # b" cr # a comment ." not printed"
."   two  spaces" 35 emit cr
1 . #x
END
t 3 'a # b\n  two  spaces#\n1 ' 'error: unknown word #x at line 3'

# The stack words.
cat >"$C/p.stk" <<'END'
1 2 3 rot . . . 1 2 over . . . 5 6 swap drop . 9 dup + . cr
END
t 0 '1 3 2 1 2 1 6 18 \n'

cat >"$C/p.stk" <<'END'
5 .
1 2
rot
END
t 3 '5 ' 'error: stack underflow at line 3'

cat >"$C/p.stk" <<'END'
3 4 < . 4 3 < . 5 5 = . 5 6 = . 0 not . 7 not . -1 not . -1 0 > . 0 -1 > . cr
END
t 0 '1 0 1 0 1 0 0 0 1 \n'

# Syntax errors: nothing runs, and N is the first offending token's line.
cat >"$C/p.stk" <<'END'
1 . cr
2 . then
END
syn 2

cat >"$C/p.stk" <<'END'
: x
  if 1 .
;
END
syn 3

cat >"$C/p.stk" <<'END'
: a 1 . ;
a ;
END
syn 2

cat >"$C/p.stk" <<'END'
: a
  : b ;
;
END
syn 2

cat >"$C/p.stk" <<'END'
1 if
: z 1 ; then
END
syn 2

cat >"$C/p.stk" <<'END'
." ok" cr
." missing
END
syn 2

printf '1 .\n."\n' >"$C/p.stk"
syn 2

cat >"$C/p.stk" <<'END'
: forever
  begin 1 .
END
syn 2

cat >"$C/p.stk" <<'END'
1 if 2
else 3
else 4 then
END
syn 3

cat >"$C/p.stk" <<'END'
begin 1
while 2
until
END
syn 3

cat >"$C/p.stk" <<'END'
1 begin
  drop repeat
END
syn 2

cat >"$C/p.stk" <<'END'
1 .
:
  dup 1 ;
END
syn 3

cat >"$C/p.stk" <<'END'
1 .
: 42 1 ;
END
syn 2

printf '1 .\n:' >"$C/p.stk"
syn 2

cat >"$C/p.stk" <<'END'
begin
  then
." oops
END
syn 2

# A missing file.
n=$((n + 1))
go "$C/no-such.stk"
if [ "$rc" != 2 ] || [ -s "$C/out" ]; then
  echo "case $n"
  exit 1
fi

# 200,000 iterations within the limit.
cat >"$C/p.stk" <<'END'
: step swap 1 + swap 1 - ;
0 200000 begin dup while step repeat drop . cr
END
t 0 '200000 \n'
rm -rf /workspace/.check
