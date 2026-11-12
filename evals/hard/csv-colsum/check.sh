cd /workspace || exit 2
LC_ALL=C
export LC_ALL
rm -rf /workspace/.check && mkdir /workspace/.check || exit 2
C=/workspace/.check
[ -f colsum.sh ] || exit 1
[ "$(sha256sum sales.csv | cut -d ' ' -f 1)" = "0bddcac7b76a4b5cb7f4f07b9bc26b93a0684a54466ea32fa1f6586da74d57df" ] || exit 1
n=0
# run STATUS FILE COLUMN [SUM]: stdout must be SUM and a LF (nothing when
# SUM is absent), and the exit status STATUS; for status 4, stderr must
# equal $C/err.
run() {
  n=$((n + 1))
  sh /workspace/colsum.sh "$2" "$3" </dev/null >"$C/out" 2>"$C/got-err"
  rc=$?
  if [ "$#" -eq 4 ]; then printf '%s\n' "$4" >"$C/want"; else : >"$C/want"; fi
  if [ "$rc" != "$1" ] || ! cmp -s "$C/out" "$C/want"; then
    echo "case $n"
    exit 1
  fi
  if [ "$1" = 4 ] && ! cmp -s "$C/got-err" "$C/err"; then
    echo "case $n"
    exit 1
  fi
}
cr=$(printf '\r')

# the prompt's example, and another column of the same file.
run 0 /workspace/sales.csv units 39
run 0 /workspace/sales.csv price_cents 31122

# CRLF records, the target is the last column.
printf 'name,qty\r\na,5\r\nb,10\r\n"c,d",-3\r\n' >"$C/crlf.csv"
run 0 "$C/crlf.csv" qty 12
printf 'a,b\r\n1,"2"\r\n3,"-4"\r\n"5",""\r\n' >"$C/crlf-quoted.csv"
run 0 "$C/crlf-quoted.csv" b -2
run 0 "$C/crlf-quoted.csv" a 9

# a quoted header containing a comma.
cat >"$C/header.csv" <<'END'
id,"total, net",x
1,10,a
2,"20",b
3,-5,"c,d"
END
run 0 "$C/header.csv" "total, net" 25
run 0 "$C/header.csv" id 6

# doubled quotes inside a quoted field.
cat >"$C/quotes.csv" <<'END'
msg,n
"he said ""hi"", ok",4
plain,6
"""",1
"a "","" b",10
END
run 0 "$C/quotes.csv" n 21

# quoted fields spanning lines, one with a CRLF inside, before the target.
printf 'note,amount\n"line one\nline two\nline three",100\n"x\r\ny",23\nz,-3\n' >"$C/multi.csv"
run 0 "$C/multi.csv" amount 120

# no line ending after the last record.
printf 'a,b\n1,2\n3,4' >"$C/noeol.csv"
run 0 "$C/noeol.csv" b 6
printf 'a,b\r\n1,2\r\n3,40' >"$C/noeol-crlf.csv"
run 0 "$C/noeol-crlf.csv" b 42

# signs, quoted signs, zeros, and a single column.
cat >"$C/signs.csv" <<'END'
v
-10
+20
"-5"
0
-0
007
END
run 0 "$C/signs.csv" v 12

# empty fields count as 0.
cat >"$C/empty.csv" <<'END'
a,b,c
1,,3
,,
"",2,""
END
run 0 "$C/empty.csv" a 1
run 0 "$C/empty.csv" b 2
run 0 "$C/empty.csv" c 3

# the first of two matching headers.
cat >"$C/dup.csv" <<'END'
x,y,x
1,2,3
4,5,6
END
run 0 "$C/dup.csv" x 5

# other columns are not checked, and large sums print in full.
cat >"$C/big.csv" <<'END'
label,n
foo,9007199254740000
bar baz,991
"q",
END
run 0 "$C/big.csv" n 9007199254740991
printf 'n\n-4503599627370496\n-4503599627370496\n' >"$C/neg.csv"
run 0 "$C/neg.csv" n -9007199254740992

# no such column (exact, case-sensitive match).
run 3 "$C/dup.csv" z
run 3 "$C/dup.csv" X
run 3 "$C/header.csv" total
run 3 "$C/header.csv" '"total, net"'

# bad values, numbered by record rather than line.
printf 'a,b\n"two\nlines",5\n3,1.5\n' >"$C/bad1.csv"
printf 'bad value in record 3\n' >"$C/err"
run 4 "$C/bad1.csv" b
printf 'a,b\nx,1\ny, 5\n' >"$C/bad2.csv"
printf 'bad value in record 3\n' >"$C/err"
run 4 "$C/bad2.csv" b
printf 'a,b\nx,-\n' >"$C/bad3.csv"
printf 'bad value in record 2\n' >"$C/err"
run 4 "$C/bad3.csv" b
printf 'a,b\r\nx,1\r\n"y\r\nz",2\r\nw,12abc\r\n' >"$C/bad4.csv"
printf 'bad value in record 4\n' >"$C/err"
run 4 "$C/bad4.csv" b

# a field count that differs from the header's.
printf 'a,b,c\n1,2,3\n4,5\n' >"$C/ragged1.csv"
run 5 "$C/ragged1.csv" a
printf 'a,b,c\n1,2,3\n4,5,6,7\n' >"$C/ragged2.csv"
run 5 "$C/ragged2.csv" c
printf 'a,b\n1,x,3\n' >"$C/ragged3.csv"
run 5 "$C/ragged3.csv" b
printf 'a,b\n1,2\n"3,4"\n' >"$C/ragged4.csv"
run 5 "$C/ragged4.csv" a

# a missing file.
run 2 "$C/no-such-file.csv" a
