cd /workspace || exit 2
LC_ALL=C
export LC_ALL
rm -rf /workspace/.check && mkdir /workspace/.check || exit 2
C=/workspace/.check
[ -f rollup.sh ] || exit 1
[ "$(ls -A logs | sort)" = "api.log
cron.log
notes.txt
worker.log" ] || exit 1
[ "$(sha256sum logs/api.log | cut -d ' ' -f 1)" = "7ebfe6f6f7fabf645d15597138c63173b34016fab70ebb5f661d4aa05fb82690" ] || exit 1
[ "$(sha256sum logs/cron.log | cut -d ' ' -f 1)" = "beb10cfe2612d869bb6b3569f4343870b766762d0570e9945b5cd3d160ee978e" ] || exit 1
[ "$(sha256sum logs/notes.txt | cut -d ' ' -f 1)" = "0826df8bf2aa9fd3c27f2fa7cae7be8aeb1359d6bf125be72fe6ce6dec18d017" ] || exit 1
[ "$(sha256sum logs/worker.log | cut -d ' ' -f 1)" = "6f842fa6351bc5ea12401e2d933db6f862942e390c6aa2f8e46d973036f369af" ] || exit 1
n=0
# run DIR: stdout must equal $C/want, with exit status 0.
run() {
  n=$((n + 1))
  sh /workspace/rollup.sh "$1" </dev/null >"$C/out" 2>/dev/null
  if [ "$?" != 0 ] || ! cmp -s "$C/out" "$C/want"; then
    echo "case $n"
    exit 1
  fi
}

# The prompt's example.
cat >"$C/want" <<'END'
2026-03-14T04 debug=0 info=0 warn=0 error=1
2026-03-14T09 debug=2 info=3 warn=1 error=1
2026-03-14T10 debug=0 info=2 warn=1 error=0
2026-03-14T13 debug=0 info=0 warn=0 error=1
2026-03-14T23 debug=0 info=1 warn=0 error=0
2026-03-15T00 debug=0 info=1 warn=1 error=1
malformed=5
END
run /workspace/logs

# Offsets across midnight, month and year ends in both directions, leap days, unusual offsets, and an empty message.
mkdir "$C/d2" || exit 2
printf '%s\n' \
  '2026-01-31T23:30:00-02:00 INFO month end forward' \
  '2026-03-01T01:00:00+03:00 WARN month end back' \
  '2026-12-31T22:00:00-03:00 ERROR year end forward' \
  '2027-01-01T00:30:00+01:00 DEBUG year end back' \
  '2028-02-29T23:00:00-02:00 INFO leap day forward' \
  '2028-03-01T00:15:00+00:30 INFO leap day back' \
  '2026-02-28T23:00:00-01:00 INFO' \
  '2026-06-15T06:00:00+05:45 WARN quarter-hour offset' \
  '2026-07-01T10:00:00+14:00 ERROR largest offset' \
  '2026-07-01T10:00:00-12:00 ERROR smallest offset' \
  '2000-01-01T03:00:00+04:00 INFO into 1999' \
  '2099-12-31T21:00:00-03:00 DEBUG into 2100' \
  '2026-05-10T12:59:59Z INFO plain' \
  '2026-05-10T12:00:00-00:00 INFO minus zero' \
  '2000-02-29T12:00:00Z WARN century leap year' \
  '2026-12-31T23:59:59-00:01 DEBUG one minute west' >"$C/d2/x.log"
printf '%s\n' \
  '2027-01-01T01:59:00+01:00 ERROR same hour, other file' \
  '2026-05-10T14:30:00+02:00 WARN x  two spaces in the message' \
  '2026-05-10T12:01:00Z ERROR message with = and : and -' >"$C/d2/y.log"
cat >"$C/want" <<'END'
1999-12-31T23 debug=0 info=1 warn=0 error=0
2000-02-29T12 debug=0 info=0 warn=1 error=0
2026-02-01T01 debug=0 info=1 warn=0 error=0
2026-02-28T22 debug=0 info=0 warn=1 error=0
2026-03-01T00 debug=0 info=1 warn=0 error=0
2026-05-10T12 debug=0 info=2 warn=1 error=1
2026-06-15T00 debug=0 info=0 warn=1 error=0
2026-06-30T20 debug=0 info=0 warn=0 error=1
2026-07-01T22 debug=0 info=0 warn=0 error=1
2026-12-31T23 debug=1 info=0 warn=0 error=0
2027-01-01T00 debug=1 info=0 warn=0 error=1
2027-01-01T01 debug=0 info=0 warn=0 error=1
2028-02-29T23 debug=0 info=1 warn=0 error=0
2028-03-01T01 debug=0 info=1 warn=0 error=0
2100-01-01T00 debug=1 info=0 warn=0 error=0
malformed=0
END
run "$C/d2"

# Invalid dates, times, offsets and levels, and blank lines.
mkdir "$C/d3" || exit 2
printf '2026-02-29T10:00:00Z INFO not a leap year\n2026-04-31T10:00:00Z INFO april has 30 days\n2026-13-01T10:00:00Z INFO month 13\n2026-00-10T10:00:00Z INFO month 0\n2026-01-00T10:00:00Z INFO day 0\n2026-01-10T24:00:00Z INFO hour 24\n2026-01-10T23:60:00Z INFO minute 60\n2026-01-10T23:59:60Z INFO second 60\n2100-01-01T00:00:00Z INFO year 2100\n1999-12-31T23:00:00Z INFO year 1999\n2100-02-29T00:00:00Z INFO not a leap year either\n2026-01-10T10:00:00+14:30 INFO beyond +14:00\n2026-01-10T10:00:00-12:30 INFO beyond -12:00\n2026-01-10T10:00:00+05:60 INFO offset minute 60\n2026-01-10T10:00:00 INFO no zone\n2026-01-10 10:00:00Z INFO space instead of T\n2026-01-10T10:00:00Z info lower case\n2026-01-10T10:00:00Z Info mixed case\n2026-01-10T10:00:00Z WARNING long level\n2026-01-10T10:00:00Z  INFO two spaces\n2026-01-10T10:00:00Z INFO \n 2026-01-10T10:00:00Z INFO leading space\n2026-01-10T10:00:00z INFO lower-case z\n2026-01-10T10:00:00+0500 INFO offset without colon\n2026-1-10T10:00:00Z INFO one-digit month\n2026-01-10T10:00:00ZINFO no space\n\n   \n\t \t\n2026-01-10T10:00:00Z ERROR the only valid line\n' >"$C/d3/bad.log"
cat >"$C/want" <<'END'
2026-01-10T10 debug=0 info=0 warn=0 error=1
malformed=26
END
run "$C/d3"

# CRLF lines are malformed.
mkdir "$C/d4" || exit 2
printf '2026-01-10T10:00:00Z INFO crlf one\r\n2026-01-10T10:05:00Z WARN lf only\n2026-01-10T10:10:00Z INFO\r\n' >"$C/d4/crlf.log"
cat >"$C/want" <<'END'
2026-01-10T10 debug=0 info=0 warn=1 error=0
malformed=2
END
run "$C/d4"

# Which files are read: dot files yes; other suffixes, upper case and subdirectories no; a file without a final LF.
mkdir "$C/d5" || exit 2
printf '%s\n' \
  '2026-04-01T08:15:00Z DEBUG hidden' \
  'not valid' >"$C/d5/.hidden.log"
printf '2026-04-01T08:00:00Z ERROR upper case suffix\n' >"$C/d5/UPPER.LOG"
printf '%s\n' \
  '2026-04-01T08:00:00Z INFO app' \
  '2026-04-01T08:30:00Z WARN app' >"$C/d5/app.log"
printf '%s\n' \
  '2026-04-01T08:00:00Z ERROR rotated' \
  'not a log line' >"$C/d5/app.log.1"
mkdir -p "$C/d5/deeper" || exit 2
printf '2026-04-01T08:00:00Z ERROR nested\n' >"$C/d5/deeper/x.log"
printf '2026-04-01T09:00:00+01:00 ERROR no final newline' >"$C/d5/last.log"
printf '2026-04-01T08:00:00Z ERROR named log\n' >"$C/d5/log"
printf '2026-04-01T08:00:00Z ERROR text\n' >"$C/d5/notes.txt"
mkdir -p "$C/d5/sub.log" || exit 2
printf '2026-04-01T08:00:00Z ERROR nested\n' >"$C/d5/sub.log/inner.log"
printf '2026-04-01T08:00:00Z ERROR named xlog\n' >"$C/d5/xlog"
cat >"$C/want" <<'END'
2026-04-01T08 debug=1 info=1 warn=1 error=1
malformed=1
END
run "$C/d5"

# An empty directory.
mkdir "$C/d6" || exit 2
cat >"$C/want" <<'END'
malformed=0
END
run "$C/d6"

# No .log files.
mkdir "$C/d7" || exit 2
printf 'x\n' >"$C/d7/old.log.gz"
printf '2026-04-01T08:00:00Z ERROR text\n' >"$C/d7/readme.txt"
cat >"$C/want" <<'END'
malformed=0
END
run "$C/d7"

# Only malformed lines.
mkdir "$C/d8" || exit 2
printf '%s\n' \
  'oops' \
  '2026-04-01T08:00:00Z TRACE x' \
  '' \
  '2026-04-01T08:00:00Z INFO ' >"$C/d8/m.log"
cat >"$C/want" <<'END'
malformed=3
END
run "$C/d8"
