cd /workspace || exit 2
LC_ALL=C
export LC_ALL
rm -rf /workspace/.check && mkdir /workspace/.check || exit 2
C=/workspace/.check
[ -f ignored.sh ] || exit 1
[ "$(find docs examples | sort | tr '\n' ' ')" = "docs docs/ignore-rules.md examples examples/paths examples/rules " ] || exit 1
[ "$(for f in $(find docs examples -type f | sort); do sha256sum "$f" | cut -d ' ' -f 1; done | sha256sum | cut -d ' ' -f 1)" = "87ee7c193cc204f89e29b244776f2efac3aed164b1cf7401fec617ab0c9dab85" ] || exit 1
n=0

# t: run the program from $C on the rules file `rules` with $C/in on
# stdin; stdout must equal $C/want, stderr must be empty, the status 0.
t() {
  n=$((n + 1))
  (cd "$C" && sh /workspace/ignored.sh rules) <"$C/in" >"$C/out" 2>"$C/err"
  rc=$?
  if [ "$rc" != 0 ] || ! cmp -s "$C/out" "$C/want" || [ -s "$C/err" ]; then
    echo "case $n"
    exit 1
  fi
}

# bad LINE: the rules file is invalid at LINE.
bad() {
  n=$((n + 1))
  printf 'a\nb\n' >"$C/in"
  (cd "$C" && sh /workspace/ignored.sh rules) <"$C/in" >"$C/out" 2>"$C/err"
  rc=$?
  printf 'rules:%s: bad pattern\n' "$1" >"$C/want"
  if [ "$rc" != 3 ] || [ -s "$C/out" ] || ! cmp -s "$C/err" "$C/want"; then
    echo "case $n"
    exit 1
  fi
}

# The prompt's example, with RULES as an absolute path.
n=$((n + 1))
cat >"$C/want" <<'END'
src/main.o
src/util.c~
target/
target/debug/app
.DS_Store
web/.DS_Store
logs/today.txt
app.log
docs/api/search.html
END
(cd / && sh /workspace/ignored.sh /workspace/examples/rules) </workspace/examples/paths >"$C/out" 2>"$C/err"
if [ "$?" != 0 ] || ! cmp -s "$C/out" "$C/want"; then
  echo "case $n"
  exit 1
fi

# A pattern without a slash matches the name at any depth.
echo 'foo' >"$C/rules"
printf 'foo\na/foo\na/b/foo\nfoo/x\nfood\na/foo/\nxfoo/y\n' >"$C/in"
printf 'foo\na/foo\na/b/foo\nfoo/x\na/foo/\n' >"$C/want"
t

# A leading slash anchors.
echo '/foo' >"$C/rules"
printf 'foo\na/foo\nfoo/bar\nbar/foo/\nfoo/\n' >"$C/in"
printf 'foo\nfoo/bar\nfoo/\n' >"$C/want"
t

# So does a slash in the middle.
echo 'a/foo' >"$C/rules"
printf 'a/foo\nb/a/foo\na/foo/z\na/foox\na/foo/\n' >"$C/in"
printf 'a/foo\na/foo/z\na/foo/\n' >"$C/want"
t

# * stays inside one segment; a directory path matches a plain rule.
echo 'doc/*.txt' >"$C/rules"
printf 'doc/a.txt\ndoc/sub/x.txt\ndoc/.txt\nx/doc/a.txt\ndoc/a.txt/\ndoc/a.txt.bak\n' >"$C/in"
printf 'doc/a.txt\ndoc/.txt\ndoc/a.txt/\n' >"$C/want"
t

# An excluded ancestor directory ignores everything below it.
echo 'doc/*' >"$C/rules"
printf 'doc\ndoc/\ndoc/a\ndoc/sub/x.txt\ndoc/sub/deep/y\n' >"$C/in"
printf 'doc/a\ndoc/sub/x.txt\ndoc/sub/deep/y\n' >"$C/want"
t

# A leading **/.
echo '**/build' >"$C/rules"
printf 'build\na/build\na/b/build/\na/b/build/x\nrebuild\nbuild.txt\na/rebuild/x\n' >"$C/in"
printf 'build\na/build\na/b/build/\na/b/build/x\n' >"$C/want"
t

# A trailing /** matches inside only.
echo 'logs/**' >"$C/rules"
printf 'logs\nlogs/\nlogs/a\nlogs/a/b\nlogs/a/\nxlogs/a\na/logs/b\n' >"$C/in"
printf 'logs/a\nlogs/a/b\nlogs/a/\n' >"$C/want"
t

# /**/ matches zero or more directories.
echo 'a/**/b' >"$C/rules"
printf 'a/b\na/x/b\na/x/y/b\na/xb\na/b/c\nb\nx/a/b\na/x/b/\n' >"$C/in"
printf 'a/b\na/x/b\na/x/y/b\na/b/c\na/x/b/\n' >"$C/want"
t

# Any other ** is a *.
printf 'foo/**.txt\na**z\n' >"$C/rules"
printf 'foo/a.txt\nfoo/b/a.txt\nfoo/.txt\naz\nabz\nx/abcz\na/z\n' >"$C/in"
printf 'foo/a.txt\nfoo/.txt\naz\nabz\nx/abcz\n' >"$C/want"
t

# Directory-only rules.
echo 'cache/' >"$C/rules"
printf 'cache\ncache/\na/cache/\na/cache\ncache/x\na/cache/y/z\n' >"$C/in"
printf 'cache/\na/cache/\ncache/x\na/cache/y/z\n' >"$C/want"
t

# A negation brings a file back.
printf '*.log\n!keep.log\n' >"$C/rules"
printf 'a.log\nkeep.log\nd/keep.log\nd/b.log\n' >"$C/in"
printf 'a.log\nd/b.log\n' >"$C/want"
t

# But not below an excluded directory; directory-only negations.
printf 'logs/\n!logs/keep.log\nout/*\n!out/keep/\n' >"$C/rules"
printf 'logs/keep.log\nlogs/a\nlogs\nout/keep/x\nout/other\nout/keep\nout/keep/\n' >"$C/in"
printf 'logs/keep.log\nlogs/a\nout/other\nout/keep\n' >"$C/want"
t

# The last matching rule decides, both ways.
printf '!a.txt\n*.txt\n*.md\n!*.md\ntmp/\n!tmp/\n' >"$C/rules"
printf 'a.txt\nb.txt\nx.md\ntmp/\ntmp/f\n' >"$C/in"
printf 'a.txt\nb.txt\n' >"$C/want"
t

# Classes: ranges, negation, a literal - and a leading ].
printf 'file[0-9].txt\n[!a-m]*.c\n[-x]z\n[]]q\n' >"$C/rules"
printf 'file1.txt\nfilex.txt\nfile10.txt\nabc.c\nzed.c\nn.c\nm.c\nsrc/q.c\n-z\nxz\nyz\n]q\n' >"$C/in"
printf 'file1.txt\nzed.c\nn.c\nsrc/q.c\n-z\nxz\n]q\n' >"$C/want"
t

# ?, * and classes never match a slash.
printf 'x/a?b\ny/*\nd/a[!x]b\n' >"$C/rules"
printf 'x/acb\nx/a/b\nx/ab\ny/z\ny/z/w\ny/\nd/a/b\nd/ayb\n' >"$C/in"
printf 'x/acb\ny/z\ny/z/w\nd/ayb\n' >"$C/want"
t

# Escapes and comments.
cat >"$C/rules" <<'END'
\#notes
\!bang
#comment
\*star
a\[b
END
printf '#notes\n!bang\nbang\n#comment\ncomment\n*star\nxstar\na[b\nab\n' >"$C/in"
printf '#notes\n!bang\n*star\na[b\n' >"$C/want"
t

# Trailing spaces go unless escaped; tabs stay; blank lines are skipped.
printf 'name\\ \nother  \n\n   \ntab\t\n' >"$C/rules"
printf 'name \nname\nother\nother \ntab\ntab\t\n' >"$C/in"
printf 'name \nother\ntab\t\n' >"$C/want"
t

# Directory paths and plain rules; repeated input lines.
printf 'tmp\n*.d\n' >"$C/rules"
printf 'tmp/\na/conf.d/\nconf.d\ntmp/x\ntmpx/\nconf.d\n' >"$C/in"
printf 'tmp/\na/conf.d/\nconf.d\ntmp/x\nconf.d\n' >"$C/want"
t

# Anchored, directory-only and ** together.
echo '/vendor/**/test/' >"$C/rules"
printf 'vendor/test/\nvendor/a/b/test/\nvendor/test\nvendor/a/test/x\nx/vendor/test/\n' >"$C/in"
printf 'vendor/test/\nvendor/a/b/test/\nvendor/a/test/x\n' >"$C/want"
t

# A negated class whose only member is ].
echo '[!]]' >"$C/rules"
printf 'a\n]\na]\n!\n' >"$C/in"
printf 'a\n!\n' >"$C/want"
t

# Adjacent globstars: none or more segments, then one or more.
echo '**/**' >"$C/rules"
printf 'a\na/b\na/b/c\nd/\n' >"$C/in"
printf 'a\na/b\na/b/c\nd/\n' >"$C/want"
t
echo 'x/**/**/y' >"$C/rules"
printf 'x/y\nx/a/y\nx/a/b/y\nx/ya\ny\nx\n' >"$C/in"
printf 'x/y\nx/a/y\nx/a/b/y\n' >"$C/want"
t

# Invalid lines: the first one is reported, nothing is printed.
printf 'ok\n# fine [\n!bad[ab\n[x\n' >"$C/rules"
bad 3
printf 'a/b\nx[!]\n' >"$C/rules"
bad 2
printf 'ok\\ \nend\\\n' >"$C/rules"
bad 2
printf 'a\n# trailing \\\n!/\n' >"$C/rules"
bad 3
printf '*.o\n!\n' >"$C/rules"
bad 2

# An unreadable rules file.
n=$((n + 1))
printf 'a\n' >"$C/in"
(cd "$C" && sh /workspace/ignored.sh no-such-file) <"$C/in" >"$C/out" 2>/dev/null
if [ "$?" != 2 ] || [ -s "$C/out" ]; then
  echo "case $n"
  exit 1
fi
rm -rf /workspace/.check
