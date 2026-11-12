cd /workspace || exit 2
LC_ALL=C
export LC_ALL
rm -rf /workspace/.check && mkdir /workspace/.check || exit 2
C=/workspace/.check
[ -f merge.sh ] || exit 1
[ "$(sha256sum base.ini | cut -d ' ' -f 1)" = "cf615e4c2a5a2390ec5d6142c0d8fe9304b5bf197a97359556103e86727e30f0" ] || exit 1
[ "$(sha256sum prod.ini | cut -d ' ' -f 1)" = "7abd4d92191a20d64c71c53545224ca8343773f9826e07a1a2d4c2825d23ca8c" ] || exit 1
n=0
# run STATUS FILE...: from /workspace, stdout must equal $C/want (empty
# for a non-zero STATUS) and the exit status STATUS; for status 3, stderr
# must equal $C/err.
run() {
  n=$((n + 1))
  want=$1
  shift
  sh /workspace/merge.sh "$@" </dev/null >"$C/out" 2>"$C/got-err"
  rc=$?
  [ "$want" = 0 ] || : >"$C/want"
  if [ "$rc" != "$want" ] || ! cmp -s "$C/out" "$C/want"; then
    echo "case $n"
    exit 1
  fi
  if [ "$want" = 3 ] && ! cmp -s "$C/got-err" "$C/err"; then
    echo "case $n"
    exit 1
  fi
}

# The prompt's example.
cat >"$C/want" <<'END'
name=shop
debug=false

[server]
host=0.0.0.0
port=80
workers=4
tls=on

[database]
url=postgres://db.internal/shop
pool=20
password=

[metrics]
endpoint=/metrics
END
run 0 base.ini prod.ini

# Case-insensitive names, values keep their case, tabs, comments, and the
# global section starting again in each file.
cat >"$C/b1.ini" <<'END'
Title = One
[Server]
Port = 1
Host=a
Log.Level-2_x = Debug
END
printf '  ;comment\n\t# tab comment\ntitle = Two\n[SERVER]\nPORT\t=\t2\t\n[db]\nuser = x\n' >"$C/b2.ini"
printf 'extra = global again\n[\tserver ]\nhost = b\n' >"$C/b3.ini"
cat >"$C/want" <<'END'
title=Two
extra=global again

[server]
port=2
host=b
log.level-2_x=Debug

[db]
user=x
END
run 0 .check/b1.ini .check/b2.ini .check/b3.ini
# A tab inside a value is kept, even when the text before it is a letter.
printf 'key = NOTE\tvalue\n[s]\nE = E\tx\n' >"$C/t1.ini"
printf 'key=NOTE\tvalue\n\n[s]\ne=E\tx\n' >"$C/want"
run 0 .check/t1.ini

# Values with =, ; and #, empty values, and trimming.
printf '[app]\nurl = http://x/?a=1&b=2\nempty =\nblank =    \nsemi = a ; not a comment # nor this\nspaced =   two  words  \t\neq==\n' >"$C/c1.ini"
cat >"$C/want" <<'END'
[app]
url=http://x/?a=1&b=2
empty=
blank=
semi=a ; not a comment # nor this
spaced=two  words
eq==
END
run 0 .check/c1.ini

# !unset, set again in a third file in its first position, and a section
# emptied by unsets.
cat >"$C/d1.ini" <<'END'
[a]
k1 = 1
k2 = 2
[b]
x = 1
[c]
only = 1
END
printf '[A]\nk1 = !unset\nnew = !unset\n[c]\nonly =   !unset  \n[b]\nx = !unset\n' >"$C/d2.ini"
cat >"$C/d3.ini" <<'END'
[a]
k1 = again
new = n
k3 = 3
[b]
x = back
END
cat >"$C/want" <<'END'
[a]
k1=again
k2=2
new=n
k3=3

[b]
x=back
END
run 0 .check/d1.ini .check/d2.ini .check/d3.ini
# The same without the third file: [b] and [c] are empty, so only [a] is left.
printf '[a]\nk2=2\n' >"$C/want"
run 0 .check/d1.ini .check/d2.ini

# Order of first appearance when a later file adds and reorders.
cat >"$C/e1.ini" <<'END'
[one]
a = 1
b = 2
[two]
c = 3
END
cat >"$C/e2.ini" <<'END'
[three]
d = 4
[two]
e = 5
c = 30
[one]
b = 20
z = 26
a = 10
END
cat >"$C/want" <<'END'
[one]
a=10
b=20
z=26

[two]
c=30
e=5

[three]
d=4
END
run 0 .check/e1.ini .check/e2.ini

# Empty results, global keys only, a value that only looks like !unset, and
# no LF at the end of a file.
printf '[x]\nk = !unset\n; c\n\n' >"$C/f1.ini"
: >"$C/want"
run 0 .check/f1.ini
: >"$C/f2.ini"
run 0 .check/f2.ini .check/f1.ini
printf 'a = 1\nb = !UNSET\nc = !unset me\n' >"$C/g1.ini"
printf 'a=1\nb=!UNSET\nc=!unset me\n' >"$C/want"
run 0 .check/g1.ini
printf '[s]\nk = v' >"$C/h1.ini"
printf 'top = 1' >"$C/h2.ini"
printf 'top=1\n\n[s]\nk=v\n' >"$C/want"
run 0 .check/h1.ini .check/h2.ini

# Malformed lines, numbered, with the file as given.
printf '[ok]\na = 1\n' >"$C/i1.ini"
i=0
for bad in 'just text' 'key name = x' '[bad name]' '[]' '= value' '[sec' '[sec] trailing' 'k:v' '[a=b]' 'ke/y = 1'; do
  i=$((i + 1))
  printf '; head\n\n%s\nz = 1\n' "$bad" >"$C/bad$i.ini"
  printf '.check/bad%d.ini:3: malformed\n' "$i" >"$C/err"
  run 3 .check/i1.ini ".check/bad$i.ini"
done
printf 'a = 1\n[x]\nb = 2\noops\n' >"$C/i2.ini"
printf '/workspace/.check/i2.ini:4: malformed\n' >"$C/err"
run 3 /workspace/.check/i2.ini
# The first problem in argument order decides.
printf '.check/i2.ini:4: malformed\n' >"$C/err"
run 3 .check/i1.ini .check/i2.ini .check/missing.ini
run 2 .check/i1.ini .check/missing.ini .check/i2.ini
run 2 .check/missing.ini
