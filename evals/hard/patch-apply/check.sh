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
# $V holds the seeded tree, each case's expected output and tree, and a
# copy of the program's output and tree taken once it has exited, which is
# what is compared. The check runs as the same user as the program, so this
# keeps an honest program from coming across the expected output; it does
# not stop one that goes looking for it.
V=$(mktemp -d) || exit 2
[ -f apply.sh ] || exit 1
[ "$(find docs | sort | tr '\n' ' ')" = "docs docs/patch.md " ] || exit 1
[ "$(for f in docs/patch.md example.patch; do sha256sum "$f" | cut -d ' ' -f 1; done | sha256sum | cut -d ' ' -f 1)" = "30f49b91fde00212839c2941d0ea5275badd7a582c5d72e76d40f124478b791b" ] || exit 1
tab=$(printf '\t')
cr=$(printf '\r')
n=0

# snapc DIR: every path under DIR and every file's hash.
snapc() {
  (cd "$1" && find . | sort && find . -type f | sort | while IFS= read -r f; do sha256sum "$f" | cut -d ' ' -f 1; done)
}
# modes DIR [dirs]: the permission string, as the first 10 characters of
# ls -ld, and the path of every file under DIR that is also a file in the
# seeded tree; with dirs, also of every directory under DIR, . included,
# that is also a directory there. A created file's or directory's mode is
# not part of the contract, and neither is an existing directory's after a
# successful run (only a failed run must leave the tree exactly as it was);
# an extra or missing path is already caught by snapc.
modes() {
  (cd "$1" && find . | sort | while IFS= read -r f; do
    if [ -f "$f" ]; then [ -f "$V/seed/$f" ] || continue
    elif [ -d "$f" ] && [ "$2" = dirs ]; then [ -d "$V/seed/$f" ] || continue
    else continue
    fi
    printf '%s %s\n' "$(ls -ld "$f" | cut -c 1-10)" "$f"
  done)
}
# snap DIR [dirs]: contents and permissions.
snap() {
  snapc "$1" && modes "$1" "$2"
}

# pt: write the patch from stdin to $C/p.patch, turning <TAB> and <CR>
# into those bytes and a final <SP> into a space.
pt() {
  sed -e "s/<TAB>/$tab/g" -e "s/<CR>/$cr/g" -e 's/<SP>$/ /' >"$C/p.patch"
}

# fresh: the expected tree $V/exp starts as the seeded tree.
fresh() {
  vrm "$V/exp" && cp -Rp "$V/seed" "$V/exp" || exit 2
}

# put FILE N TEXT / ins FILE N TEXT: in $V/exp/FILE, replace line N with
# TEXT, or insert TEXT after line N.
put() {
  T=$3 awk -v n="$2" 'NR == n { print ENVIRON["T"]; next } { print }' "$V/exp/$1" >"$V/x" && cat "$V/x" >"$V/exp/$1"
}
ins() {
  T=$3 awk -v n="$2" '{ print } NR == n { print ENVIRON["T"] }' "$V/exp/$1" >"$V/x" && cat "$V/x" >"$V/exp/$1"
}

# go: start case n + 1: a fresh copy of the seeded tree, and a fresh, empty
# $C/run, where the program runs. A case's expected output and expected
# tree are made only after the program has exited, and only in $V.
go() {
  n=$((n + 1))
  rm -rf "$C/run" "$C/out" "$C/err" &&
    vrm "$V/exp" "$V/want" "$V/werr" "$V/x" "$V/out" "$V/err" "$V/tree" &&
    mkdir "$C/run" || exit 2
  rm -rf /workspace/tree && cp -Rp "$V/seed" /workspace/tree || exit 2
}

# keep: once the program has exited, copy its stdout, stderr and tree to
# $V. A tree that cannot be copied fails the case.
keep() {
  for f in out err; do
    [ ! -e "$C/$f" ] || cp "$C/$f" "$V/$f" || exit 2
  done
  if [ -e /workspace/tree ] && ! cp -Rp /workspace/tree "$V/tree" 2>/dev/null; then
    echo "case $n"
    exit 1
  fi
}

# r: start a case and apply a copy of $C/p.patch, from $C/run.
r() {
  go
  cp "$C/p.patch" "$C/run/p.patch" || exit 2
  (cd "$C/run" && sh /workspace/apply.sh p.patch) </dev/null >"$C/out" 2>"$C/err"
  rc=$?
  keep
}

# t STATUS OUT [ERR]: after r and the expected tree $V/exp, stdout must
# be the printf format OUT, the exit status STATUS, and the tree must
# equal $V/exp (on failure, directory modes included); on failure, stderr
# must be the line ERR.
t() {
  d=
  [ "$1" = 0 ] || d=dirs
  printf '%b' "$2" >"$V/want"
  if [ "$rc" != "$1" ] || ! cmp -s "$V/out" "$V/want" || [ "$(snap "$V/tree" $d)" != "$(snap "$V/exp" $d)" ]; then
    echo "case $n"
    exit 1
  fi
  if [ $# -ge 3 ]; then
    printf '%s\n' "$3" >"$V/werr"
    if ! cmp -s "$V/err" "$V/werr"; then
      echo "case $n"
      exit 1
    fi
  fi
}

# The tree as seeded.
mkdir -p "$V/seed/conf" "$V/seed/data" "$V/seed/src" || exit 2
# conf/ has an uncommon mode, which a failed run must leave as it is.
chmod 0750 "$V/seed/conf" || exit 2
cat >"$V/seed/README.md" <<'TREE_END'
# tinyhttp

A small HTTP server for local testing.

## Building

Run `make` in the top directory. The binary is written to `bin/tinyhttp`.

## Running

    bin/tinyhttp -p 8080 -d public

## License

MIT
TREE_END
printf '# tinyhttp settings\nport = 8000\nroot = public\nlog = off' >"$V/seed/conf/app.conf"
cat >"$V/seed/data/readings.txt" <<'TREE_END'
# hourly readings, sensor 4
12
12
12
13
15
18
21
24
22
19
21
24
17
16
15
14
15
14
13
13
13
12
11
11
TREE_END
cat >"$V/seed/src/config.h" <<'TREE_END'
#ifndef CONFIG_H
#define CONFIG_H

#define DEFAULT_PORT 8000
#define DEFAULT_ROOT "."

#endif
TREE_END
cat >"$V/seed/src/server.c" <<'TREE_END'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "config.h"

static int port = DEFAULT_PORT;
static const char *root = DEFAULT_ROOT;

static void usage(const char *name)
{
	fprintf(stderr, "usage: %s [-p port] [-d dir]\n", name);
	exit(2);
}

static int parse_port(const char *s)
{
	int p = atoi(s);
	if (p <= 0 || p > 65535)
		return -1;
	return p;
}

int main(int argc, char **argv)
{
	int i;

	for (i = 1; i < argc; i++) {
		if (strcmp(argv[i], "-p") == 0 && i + 1 < argc) {
			port = parse_port(argv[++i]);
			if (port < 0)
				usage(argv[0]);
		} else if (strcmp(argv[i], "-d") == 0 && i + 1 < argc) {
			root = argv[++i];
		} else {
			usage(argv[0]);
		}
	}
	printf("serving %s on port %d\n", root, port);
	return 0;
}
TREE_END

# The prompt's example, with PATCH as an absolute path.
go
(cd / && sh /workspace/apply.sh /workspace/example.patch) </dev/null >"$C/out" 2>"$C/err"
rc=$?
keep
printf 'patched src/config.h\npatched src/server.c\ncreated docs/USAGE.md\n' >"$V/want"
# Contents against a fixed hash; the patched files' permissions against
# the seed.
fresh
if [ "$rc" != 0 ] || ! cmp -s "$V/out" "$V/want" || [ "$(snapc "$V/tree" | sha256sum | cut -d ' ' -f 1)" != "ae8625b4159dda3a246c15f487cd150019fe40c77e014cf5d395d07c67832836" ] || [ "$(modes "$V/tree")" != "$(modes "$V/exp")" ]; then
  echo "case $n"
  exit 1
fi

# An earlier hunk's line-count change moves the next hunk's position.
pt <<'END'
--- a/data/readings.txt
+++ b/data/readings.txt
@@ -1 +1,2 @@
 # hourly readings, sensor 4
+# calibrated 2026-10-30
@@ -3,2 +4,2 @@
 12
-12
+10
END
r
fresh
put data/readings.txt 4 10
ins data/readings.txt 1 '# calibrated 2026-10-30'
t 0 'patched data/readings.txt\n'

# A hunk found 3 lines late moves the next one, which is found 2 early.
pt <<'END'
--- a/data/readings.txt
+++ b/data/readings.txt
@@ -3,3 +3,4 @@
 15
+16
 18
 21
@@ -20,2 +21,2 @@
 13
-13
+14
END
r
fresh
put data/readings.txt 22 14
ins data/readings.txt 6 16
t 0 'patched data/readings.txt\n'

# Matches 2 after and 2 before: after wins.
pt <<'END'
--- a/data/readings.txt
+++ b/data/readings.txt
@@ -10,2 +10,2 @@
 21
-24
+25
END
r
fresh
put data/readings.txt 13 25
t 0 'patched data/readings.txt\n'

# An expected position past the end of the file.
pt <<'END'
--- a/README.md
+++ b/README.md
@@ -40 +40 @@
-MIT
+MIT License
END
r
fresh
put README.md 15 'MIT License'
t 0 'patched README.md\n'

# Adding the final newline.
pt <<'END'
--- a/conf/app.conf
+++ b/conf/app.conf
@@ -3,2 +3,2 @@
 root = public
-log = off
\ No newline at end of file
+log = off
END
r
fresh
printf '# tinyhttp settings\nport = 8000\nroot = public\nlog = off\n' >"$V/exp/conf/app.conf"
t 0 'patched conf/app.conf\n'

# Removing it.
pt <<'END'
--- a/src/config.h
+++ b/src/config.h
@@ -5,3 +5,3 @@
 #define DEFAULT_ROOT "."
<SP>
-#endif
+#endif
\ No newline at end of file
END
r
fresh
printf '#ifndef CONFIG_H\n#define CONFIG_H\n\n#define DEFAULT_PORT 8000\n#define DEFAULT_ROOT "."\n\n#endif' >"$V/exp/src/config.h"
t 0 'patched src/config.h\n'

# An insertion goes after line OLDSTART; the missing final newline stays.
pt <<'END'
--- a/conf/app.conf
+++ b/conf/app.conf
@@ -2,0 +3 @@
+host = 127.0.0.1
END
r
fresh
printf '# tinyhttp settings\nport = 8000\nhost = 127.0.0.1\nroot = public\nlog = off' >"$V/exp/conf/app.conf"
t 0 'patched conf/app.conf\n'

# Creating a file and its parent directories.
pt <<'END'
diff --git a/new/dir/f.txt b/new/dir/f.txt
new file mode 100644
--- /dev/null
+++ b/new/dir/f.txt
@@ -0,0 +1,2 @@
+first
+second
END
r
fresh
mkdir -p "$V/exp/new/dir"
printf 'first\nsecond\n' >"$V/exp/new/dir/f.txt"
t 0 'created new/dir/f.txt\n'

# Deleting files; the emptied directory stays.
{
  cat <<'END'
--- a/conf/app.conf
+++ /dev/null
@@ -1,4 +0,0 @@
-# tinyhttp settings
-port = 8000
-root = public
-log = off
\ No newline at end of file
--- a/data/readings.txt
+++ /dev/null
@@ -1,25 +0,0 @@
END
  sed 's/^/-/' "$V/seed/data/readings.txt"
} | pt
r
fresh
rm "$V/exp/conf/app.conf" "$V/exp/data/readings.txt"
t 0 'deleted conf/app.conf\ndeleted data/readings.txt\n'

# Header counts left out, and text after the closing @@.
pt <<'END'
--- a/src/config.h
+++ b/src/config.h
@@ -4 +4 @@ settings
-#define DEFAULT_PORT 8000
+#define DEFAULT_PORT 9000
@@ -7 +7,2 @@
 #endif
+/* end of config.h */
END
r
fresh
put src/config.h 4 '#define DEFAULT_PORT 9000'
ins src/config.h 7 '/* end of config.h */'
t 0 'patched src/config.h\n'

# Preamble and other lines between sections; timestamps after a tab.
pt <<'END'
From the release branch.
diff --git a/README.md b/README.md
index 0123456..789abcd 100644
--- a/README.md
+++ b/README.md
@@ -15 +15 @@
-MIT
+MIT License
diff --git a/src/config.h b/src/config.h
similarity index 90%
--- a/src/config.h<TAB>2026-11-01 08:00:00.000000000 +0000
+++ b/src/config.h<TAB>2026-11-01 08:05:00.000000000 +0000
@@ -4 +4 @@
-#define DEFAULT_PORT 8000
+#define DEFAULT_PORT 8443
END
r
fresh
put README.md 15 'MIT License'
put src/config.h 4 '#define DEFAULT_PORT 8443'
t 0 'patched README.md\npatched src/config.h\n'

# CRLF lines are patched byte for byte. For these two cases the tree also
# holds a CRLF file.
printf '[server]\r\nport=8000\r\nroot=public\r\n\r\n[log]\r\nlevel=info\r\n' >"$V/seed/conf/win.ini"
pt <<'END'
--- a/conf/win.ini
+++ b/conf/win.ini
@@ -5,2 +5,3 @@
 [log]<CR>
-level=info<CR>
+level=debug<CR>
+file=server.log<CR>
END
r
fresh
printf '[server]\r\nport=8000\r\nroot=public\r\n\r\n[log]\r\nlevel=debug\r\nfile=server.log\r\n' >"$V/exp/conf/win.ini"
t 0 'patched conf/win.ini\n'

# A line without its CR does not match.
pt <<'END'
--- a/conf/win.ini
+++ b/conf/win.ini
@@ -2 +2 @@
-port=8000
+port=9000
END
r
fresh
t 1 '' 'hunk 1 of conf/win.ini failed'
rm "$V/seed/conf/win.ini"

# Patched files keep their permissions, whatever they are. For these two
# cases the tree also has two files with uncommon modes.
printf '#!/bin/sh\necho starting\nexec bin/tinyhttp -p 8000 -d public\n' >"$V/seed/run.sh" && chmod 0750 "$V/seed/run.sh" || exit 2
printf '[auth]\nuser = admin\ntoken = changeme\n' >"$V/seed/conf/private.ini" && chmod 0640 "$V/seed/conf/private.ini" || exit 2
pt <<'END'
--- a/run.sh
+++ b/run.sh
@@ -2,2 +2,2 @@
 echo starting
-exec bin/tinyhttp -p 8000 -d public
+exec bin/tinyhttp -p 9000 -d public
--- a/conf/private.ini
+++ b/conf/private.ini
@@ -3 +3 @@
-token = changeme
+token = s3cret
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-# tinyhttp
+# tinyhttp 2
END
r
fresh
put run.sh 3 'exec bin/tinyhttp -p 9000 -d public'
put conf/private.ini 3 'token = s3cret'
put README.md 1 '# tinyhttp 2'
t 0 'patched run.sh\npatched conf/private.ini\npatched README.md\n'

# A later failure leaves both files, and their permissions, as they were.
pt <<'END'
--- a/run.sh
+++ b/run.sh
@@ -3 +3 @@
-exec bin/tinyhttp -p 8000 -d public
+exec bin/tinyhttp -p 9000 -d public
--- a/conf/private.ini
+++ b/conf/private.ini
@@ -2 +2 @@
-user = admin
+user = root
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-# no such line
+# tinyhttp 2
END
r
fresh
t 1 '' 'hunk 1 of README.md failed'

# A deletion whose hunks leave lines fails, and the file keeps its content
# and its permissions.
pt <<'END'
--- a/conf/private.ini
+++ /dev/null
@@ -1,2 +0,0 @@
-[auth]
-user = admin
END
r
fresh
t 1 '' 'cannot patch conf/private.ini'

# A valid deletion followed by a failing section: the file stays, with its
# content and its permissions.
pt <<'END'
--- a/conf/private.ini
+++ /dev/null
@@ -1,3 +0,0 @@
-[auth]
-user = admin
-token = changeme
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-# no such line
+# tinyhttp 2
END
r
fresh
t 1 '' 'hunk 1 of README.md failed'
rm "$V/seed/run.sh" "$V/seed/conf/private.ini"

# Malformed: a second no-newline marker straight after the first.
pt <<'END'
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+first
\ No newline at end of file
\ No newline at end of file
+second
END
r
fresh
t 2 '' 'malformed patch at line 6'

# A failing hunk in the third section leaves the whole tree untouched.
pt <<'END'
--- /dev/null
+++ b/extra/notes.txt
@@ -0,0 +1 @@
+note
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-# tinyhttp
+# tinyhttp 2
--- a/src/server.c
+++ b/src/server.c
@@ -7,2 +7,3 @@
 static int port = DEFAULT_PORT;
 static const char *root = DEFAULT_ROOT;
+static int verbose = 0;
@@ -12,2 +13,2 @@
-<TAB>fprintf(stderr, "usage: %s [-p port]\n", name);
+<TAB>fprintf(stderr, "usage: %s [-v] [-p port]\n", name);
 <TAB>exit(2);
END
r
fresh
t 1 '' 'hunk 2 of src/server.c failed'

# Creating a file that exists.
pt <<'END'
--- a/src/config.h
+++ b/src/config.h
@@ -4 +4 @@
-#define DEFAULT_PORT 8000
+#define DEFAULT_PORT 9000
--- /dev/null
+++ b/README.md
@@ -0,0 +1 @@
+hello
END
r
fresh
t 1 '' 'cannot patch README.md'

# Patching a missing file.
pt <<'END'
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-# tinyhttp
+# tinyhttp 2
--- a/src/missing.c
+++ b/src/missing.c
@@ -1 +1 @@
-x
+y
END
r
fresh
t 1 '' 'cannot patch src/missing.c'

# A deletion that would leave lines behind.
pt <<'END'
--- a/src/config.h
+++ /dev/null
@@ -1,2 +0,0 @@
-#ifndef CONFIG_H
-#define CONFIG_H
END
r
fresh
t 1 '' 'cannot patch src/config.h'

# Malformed: a hunk with too few lines runs into the next header.
pt <<'END'
--- a/src/config.h
+++ b/src/config.h
@@ -4,3 +4,3 @@
-#define DEFAULT_PORT 8000
+#define DEFAULT_PORT 9000
 #define DEFAULT_ROOT "."
@@ -7 +7 @@
 #endif
END
r
fresh
t 2 '' 'malformed patch at line 7'

# Malformed: a body line beyond the counts.
pt <<'END'
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-# tinyhttp
+# tinyhttp 2
<SP>
END
r
fresh
t 2 '' 'malformed patch at line 6'

# Malformed: +++ without ---.
pt <<'END'
diff --git a/README.md b/README.md
+++ b/README.md
@@ -1 +1 @@
-# tinyhttp
+# tinyhttp 2
END
r
fresh
t 2 '' 'malformed patch at line 2'

# Malformed: the patch ends inside a hunk.
pt <<'END'
--- a/README.md
+++ b/README.md
@@ -1,2 +1,2 @@
-# tinyhttp
+# tinyhttp 2
END
r
fresh
t 2 '' 'malformed patch at line 3'

# Malformed beats a failing hunk earlier in the patch.
pt <<'END'
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-# no such line
+# tinyhttp 2
--- a/src/config.h
+++ b/src/config.h
@@ -x +1 @@
END
r
fresh
t 2 '' 'malformed patch at line 8'

# An unreadable patch.
go
(cd "$C/run" && sh /workspace/apply.sh no-such.patch) </dev/null >"$C/out" 2>/dev/null
rc=$?
keep
fresh
if [ "$rc" != 2 ] || [ -s "$V/out" ] || [ "$(snap "$V/tree" dirs)" != "$(snap "$V/exp" dirs)" ]; then
  echo "case $n"
  exit 1
fi
rm -rf /workspace/tree && cp -Rp "$V/seed" /workspace/tree && rm -rf /workspace/.check
