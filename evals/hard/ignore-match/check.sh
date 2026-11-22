cd /workspace || exit 2
LC_ALL=C
export LC_ALL
# The cleanup is armed before anything is made, so it runs on every exit
# the shell can catch, however far setup got; sandbox teardown covers the
# rest. It removes $V, the verifier's directory outside the workspace, and
# the workspace's .check (quietly, as that holds what the program left).
# vrm PATH...: remove verifier-owned paths. Their copies of the program's
# tree keep its modes for the comparison, so each directory under them is
# first made the verifier's to enter and empty; find does not follow links,
# and nothing under the workspace is changed.
V=
vrm() {
  for p in "$@"; do
    if [ -d "$p" ] && [ ! -L "$p" ]; then
      find "$p" -type d -exec chmod u+rwx {} \;
    fi
    rm -rf "$p" || return 1
  done
}
trap 'if [ -n "$V" ]; then vrm "$V"; fi; rm -rf /workspace/.check 2>/dev/null' 0
trap 'exit 2' HUP INT TERM
rm -rf /workspace/.check && mkdir /workspace/.check || exit 2
C=/workspace/.check
# $V holds each case's expected output and a copy of the program's output
# taken once it has exited, which is what is compared.
# The check runs as the same user as the program, so this keeps an honest
# program from coming across the expected output; it does not stop one that
# goes looking for it.
V=$(mktemp -d) || exit 2
[ -f ignored.sh ] || exit 1
[ "$(find docs examples | sort | tr '\n' ' ')" = "docs docs/ignore-rules.md examples examples/paths examples/rules " ] || exit 1
[ "$(for f in $(find docs examples -type f | sort); do sha256sum "$f" | cut -d ' ' -f 1; done | sha256sum | cut -d ' ' -f 1)" = "9a3cc38ba3bdbdc3cc5b3f3abe94e040dc726a365c244653a3c47dd777f41af3" ] || exit 1
n=0

# go: start case n + 1 in a fresh, empty $C/run, where the program runs.
# A case's expected output is written only after the program has exited,
# and only in $V.
go() {
  n=$((n + 1))
  rm -rf "$C/run" "$C/out" "$C/err" "$V/want" "$V/out" "$V/err" && mkdir "$C/run" || exit 2
}

# keep: once the program has exited, copy its stdout and stderr to $V.
keep() {
  for f in out err; do
    [ ! -e "$C/$f" ] || cp "$C/$f" "$V/$f" || exit 2
  done
}

# r: start a case and run the program from $C/run on a copy of $C/rules
# named `rules`, with $C/in on stdin.
r() {
  go
  cp "$C/rules" "$C/run/rules" || exit 2
  (cd "$C/run" && sh /workspace/ignored.sh rules) <"$C/in" >"$C/out" 2>"$C/err"
  rc=$?
  keep
}

# t: after r, stdout must equal $V/want, stderr must be empty, the status 0.
t() {
  if [ "$rc" != 0 ] || ! cmp -s "$V/out" "$V/want" || [ -s "$V/err" ]; then
    echo "case $n"
    exit 1
  fi
}

# bad LINE: the rules file is invalid at LINE.
bad() {
  printf 'a\nb\n' >"$C/in"
  r
  printf 'rules:%s: bad pattern\n' "$1" >"$V/want"
  if [ "$rc" != 3 ] || [ -s "$V/out" ] || ! cmp -s "$V/err" "$V/want"; then
    echo "case $n"
    exit 1
  fi
}

# The prompt's example, with RULES as an absolute path.
go
(cd / && sh /workspace/ignored.sh /workspace/examples/rules) </workspace/examples/paths >"$C/out" 2>"$C/err"
rc=$?
keep
cat >"$V/want" <<'END'
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
if [ "$rc" != 0 ] || ! cmp -s "$V/out" "$V/want"; then
  echo "case $n"
  exit 1
fi

# A pattern without a slash matches the name at any depth.
echo 'foo' >"$C/rules"
printf 'foo\na/foo\na/b/foo\nfoo/x\nfood\na/foo/\nxfoo/y\n' >"$C/in"
r
printf 'foo\na/foo\na/b/foo\nfoo/x\na/foo/\n' >"$V/want"
t

# A leading slash anchors.
echo '/foo' >"$C/rules"
printf 'foo\na/foo\nfoo/bar\nbar/foo/\nfoo/\n' >"$C/in"
r
printf 'foo\nfoo/bar\nfoo/\n' >"$V/want"
t

# So does a slash in the middle.
echo 'a/foo' >"$C/rules"
printf 'a/foo\nb/a/foo\na/foo/z\na/foox\na/foo/\n' >"$C/in"
r
printf 'a/foo\na/foo/z\na/foo/\n' >"$V/want"
t

# * stays inside one segment; a directory path matches a plain rule.
echo 'doc/*.txt' >"$C/rules"
printf 'doc/a.txt\ndoc/sub/x.txt\ndoc/.txt\nx/doc/a.txt\ndoc/a.txt/\ndoc/a.txt.bak\n' >"$C/in"
r
printf 'doc/a.txt\ndoc/.txt\ndoc/a.txt/\n' >"$V/want"
t

# An excluded ancestor directory ignores everything below it.
echo 'doc/*' >"$C/rules"
printf 'doc\ndoc/\ndoc/a\ndoc/sub/x.txt\ndoc/sub/deep/y\n' >"$C/in"
r
printf 'doc/a\ndoc/sub/x.txt\ndoc/sub/deep/y\n' >"$V/want"
t

# A leading **/.
echo '**/build' >"$C/rules"
printf 'build\na/build\na/b/build/\na/b/build/x\nrebuild\nbuild.txt\na/rebuild/x\n' >"$C/in"
r
printf 'build\na/build\na/b/build/\na/b/build/x\n' >"$V/want"
t

# A trailing /** matches inside only.
echo 'logs/**' >"$C/rules"
printf 'logs\nlogs/\nlogs/a\nlogs/a/b\nlogs/a/\nxlogs/a\na/logs/b\n' >"$C/in"
r
printf 'logs/a\nlogs/a/b\nlogs/a/\n' >"$V/want"
t

# /**/ matches zero or more directories.
echo 'a/**/b' >"$C/rules"
printf 'a/b\na/x/b\na/x/y/b\na/xb\na/b/c\nb\nx/a/b\na/x/b/\n' >"$C/in"
r
printf 'a/b\na/x/b\na/x/y/b\na/b/c\na/x/b/\n' >"$V/want"
t

# Any other ** is a *.
printf 'foo/**.txt\na**z\n' >"$C/rules"
printf 'foo/a.txt\nfoo/b/a.txt\nfoo/.txt\naz\nabz\nx/abcz\na/z\n' >"$C/in"
r
printf 'foo/a.txt\nfoo/.txt\naz\nabz\nx/abcz\n' >"$V/want"
t

# Directory-only rules.
echo 'cache/' >"$C/rules"
printf 'cache\ncache/\na/cache/\na/cache\ncache/x\na/cache/y/z\n' >"$C/in"
r
printf 'cache/\na/cache/\ncache/x\na/cache/y/z\n' >"$V/want"
t

# A negation brings a file back.
printf '*.log\n!keep.log\n' >"$C/rules"
printf 'a.log\nkeep.log\nd/keep.log\nd/b.log\n' >"$C/in"
r
printf 'a.log\nd/b.log\n' >"$V/want"
t

# But not below an excluded directory; directory-only negations.
printf 'logs/\n!logs/keep.log\nout/*\n!out/keep/\n' >"$C/rules"
printf 'logs/keep.log\nlogs/a\nlogs\nout/keep/x\nout/other\nout/keep\nout/keep/\n' >"$C/in"
r
printf 'logs/keep.log\nlogs/a\nout/other\nout/keep\n' >"$V/want"
t

# The last matching rule decides, both ways.
printf '!a.txt\n*.txt\n*.md\n!*.md\ntmp/\n!tmp/\n' >"$C/rules"
printf 'a.txt\nb.txt\nx.md\ntmp/\ntmp/f\n' >"$C/in"
r
printf 'a.txt\nb.txt\n' >"$V/want"
t

# Classes: ranges, negation, a literal - and a leading ].
printf 'file[0-9].txt\n[!a-m]*.c\n[-x]z\n[]]q\n' >"$C/rules"
printf 'file1.txt\nfilex.txt\nfile10.txt\nabc.c\nzed.c\nn.c\nm.c\nsrc/q.c\n-z\nxz\nyz\n]q\n' >"$C/in"
r
printf 'file1.txt\nzed.c\nn.c\nsrc/q.c\n-z\nxz\n]q\n' >"$V/want"
t

# ?, * and classes never match a slash.
printf 'x/a?b\ny/*\nd/a[!x]b\n' >"$C/rules"
printf 'x/acb\nx/a/b\nx/ab\ny/z\ny/z/w\ny/\nd/a/b\nd/ayb\n' >"$C/in"
r
printf 'x/acb\ny/z\ny/z/w\nd/ayb\n' >"$V/want"
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
r
printf '#notes\n!bang\n*star\na[b\n' >"$V/want"
t

# Trailing spaces go unless escaped; tabs stay; blank lines are skipped.
printf 'name\\ \nother  \n\n   \ntab\t\n' >"$C/rules"
printf 'name \nname\nother\nother \ntab\ntab\t\n' >"$C/in"
r
printf 'name \nother\ntab\t\n' >"$V/want"
t

# Directory paths and plain rules; repeated input lines.
printf 'tmp\n*.d\n' >"$C/rules"
printf 'tmp/\na/conf.d/\nconf.d\ntmp/x\ntmpx/\nconf.d\n' >"$C/in"
r
printf 'tmp/\na/conf.d/\nconf.d\ntmp/x\nconf.d\n' >"$V/want"
t

# Anchored, directory-only and ** together.
echo '/vendor/**/test/' >"$C/rules"
printf 'vendor/test/\nvendor/a/b/test/\nvendor/test\nvendor/a/test/x\nx/vendor/test/\n' >"$C/in"
r
printf 'vendor/test/\nvendor/a/b/test/\nvendor/a/test/x\n' >"$V/want"
t

# A negated class whose only member is ].
echo '[!]]' >"$C/rules"
printf 'a\n]\na]\n!\n' >"$C/in"
r
printf 'a\n!\n' >"$V/want"
t

# Adjacent globstars: none or more segments, then one or more.
echo '**/**' >"$C/rules"
printf 'a\na/b\na/b/c\nd/\n' >"$C/in"
r
printf 'a\na/b\na/b/c\nd/\n' >"$V/want"
t
echo 'x/**/**/y' >"$C/rules"
printf 'x/y\nx/a/y\nx/a/b/y\nx/ya\ny\nx\n' >"$C/in"
r
printf 'x/y\nx/a/y\nx/a/b/y\n' >"$V/want"
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
go
printf 'a\n' >"$C/in"
(cd "$C/run" && sh /workspace/ignored.sh no-such-file) <"$C/in" >"$C/out" 2>/dev/null
rc=$?
keep
if [ "$rc" != 2 ] || [ -s "$V/out" ]; then
  echo "case $n"
  exit 1
fi

# A lone ** in an anchored rule matches one segment, like /*.
printf '/**\n!a/\n' >"$C/rules"
printf 'a/b\na/\nc\nd/e\n' >"$C/in"
r
printf 'c\nd/e\n' >"$V/want"
t

# A segment of three stars is not exactly **: it stays in one segment.
echo 'a/***/b' >"$C/rules"
printf 'a/b\na/x/b\na/x/y/b\n' >"$C/in"
r
printf 'a/x/b\n' >"$V/want"
t

# A range whose start is above its end covers nothing.
printf 'x[z-a]y\n*.k\n' >"$C/rules"
printf 'xay\nxzy\nxmy\nx-y\na.k\n' >"$C/in"
r
printf 'a.k\n' >"$V/want"
t

# The error names RULES exactly as given, backslash included.
go
printf 'ok\n[x\n' >"$C/run/r\\t"
printf 'a\n' >"$C/in"
(cd "$C/run" && sh /workspace/ignored.sh 'r\t') <"$C/in" >"$C/out" 2>"$C/err"
rc=$?
keep
printf 'r\\t:2: bad pattern\n' >"$V/want"
if [ "$rc" != 3 ] || [ -s "$V/out" ] || ! cmp -s "$V/err" "$V/want"; then
  echo "case $n"
  exit 1
fi
rm -rf /workspace/.check
