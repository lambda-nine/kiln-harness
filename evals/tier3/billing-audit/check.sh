cd /workspace || exit 2
LC_ALL=C
export LC_ALL
N=32
B=/workspace/billing
C=$(mktemp -d) || exit 2
trap 'rm -rf "$C"' EXIT
# An absolute path, so that the paths under it survive a cd.
C=$(cd "$C" && pwd -P) || exit 2
command -v cmp >/dev/null 2>&1 || {
  echo "check: cmp is not available" >&2
  exit 2
}

# all: a precondition failed, so no case counts.
all() {
  echo "cases 0 of $N failed all"
  exit 1
}

# Where setsid exists, each run gets its own session and process group,
# so that whatever it starts can be killed with it.
ss=
if command -v setsid >/dev/null 2>&1; then ss=setsid; fi

# stop PID: kill run PID, and its whole group where it has one. (No
# "--" before the group: dash's kill rejects it.)
stop() {
  if [ -n "$ss" ]; then
    kill -9 "-$1" 2>/dev/null || kill -9 "$1" 2>/dev/null
  else
    kill -9 "$1" 2>/dev/null
  fi
}

# go LIMIT NAME DIR CMD...: run CMD in DIR, in the background, with no
# input and its output in $C/NAME.out and $C/NAME.err, and kill it after
# LIMIT seconds. Sets rc to its exit status; a killed run's status is not
# 0. When the run ends, whatever is left in its group is killed. The run
# writes $C/NAME.started just before CMD starts; without it the run never
# started, and the check stops without a score.
go() {
  lim=$1
  name=$2
  dir=$3
  shift 3
  (cd "$dir" && exec $ss sh -c ': >"$1" && shift && exec "$@"' sh "$C/$name.started" "$@") \
    </dev/null >"$C/$name.out" 2>"$C/$name.err" &
  pid=$!
  (
    t=
    trap 'if [ -n "$t" ]; then kill "$t" 2>/dev/null; wait "$t" 2>/dev/null; fi' 0
    trap 'exit 0' HUP INT TERM
    sleep "$lim" &
    t=$!
    wait "$t" || exit 0
    t=
    kill -0 "$pid" 2>/dev/null || exit 0
    stop "$pid"
  ) </dev/null >/dev/null 2>&1 &
  wd=$!
  wait "$pid" 2>/dev/null
  rc=$?
  kill "$wd" 2>/dev/null
  wait "$wd" 2>/dev/null
  if [ -n "$ss" ]; then kill -9 "-$pid" 2>/dev/null; fi
  if [ ! -f "$C/$name.started" ]; then
    echo "check: the $name run did not acknowledge its start" >&2
    exit 2
  fi
}

# The protected files are unchanged, and the visible tests pass.
[ -f "$B/bin/bill" ] || all
[ "$(find billing/docs billing/tests billing/examples billing/ISSUES.md 2>/dev/null | sort | tr '\n' ' ')" = "billing/ISSUES.md billing/docs billing/docs/MANUAL.md billing/examples billing/examples/shop billing/examples/shop-2026-03.txt billing/examples/shop/accounts billing/examples/shop/credits billing/examples/shop/discounts billing/examples/shop/plans billing/examples/shop/tiers billing/examples/shop/usage billing/examples/studio billing/examples/studio-2026-05.txt billing/examples/studio/accounts billing/examples/studio/credits billing/examples/studio/discounts billing/examples/studio/plans billing/examples/studio/tiers billing/examples/studio/usage billing/tests billing/tests/run.sh " ] || all
[ "$(for f in $(find billing/docs billing/tests billing/examples billing/ISSUES.md -type f 2>/dev/null | sort); do sha256sum "$f" | cut -d ' ' -f 1; done | sha256sum | cut -d ' ' -f 1)" = "9021e23769c276611b7b2d25f143766eb9e1943f269f161d49a56a84d20535ee" ] || all
# Each limit is the one the prompt states plus 1 second for the launch.
go 16 tests / sh "$B/tests/run.sh"
[ "$rc" = 0 ] || all

# Each case runs bill once, in order, on a fresh data directory, and
# fails if that run takes more than 5 seconds (6 with the launch
# allowance). Every case runs.
n=0
pass=0
bad=
mkdir "$C/std" || exit 2
cat >"$C/std/plans" <<'E'
basic 30.00 flat 0
metered 5.00 graduated 0
bulk 20.00 volume 0
trial 10.00 graduated 2
E
cat >"$C/std/tiers" <<'E'
metered 100 0.10
metered 1000 0.08
metered * 0.05
bulk 100 0.10
bulk 1000 0.08
bulk * 0.05
trial 10 0.20
trial * 0.10
E

# new [std]: start the next case with an empty data directory, or one
# holding the standard plans and tiers above.
new() {
  n=$((n + 1))
  D=$C/$n
  mkdir -p "$D/data" || exit 2
  if [ "$1" = std ]; then cp "$C/std/plans" "$C/std/tiers" "$D/data/" || exit 2; fi
}

# f FILE: write the case's data file FILE from standard input.
f() {
  cat >"$D/data/$1"
}

# r PERIOD: run bill data PERIOD in the case's directory.
r() {
  go 6 "$n" "$D" sh "$B/bin/bill" data "$1"
}

# w: the case passes if bill exited 0 (a run the watchdog killed did
# not) and printed exactly standard input. The expected output is
# written only now, after bill has exited, and outside the case's
# directory. Output cmp can't read fails the case; a cmp that can't run
# stops the check.
w() {
  cat >"$C/want"
  s=1
  if [ "$rc" = 0 ]; then
    cmp -s "$C/$n.out" "$C/want"
    s=$?
  fi
  case $s in
    0) pass=$((pass + 1)) ;;
    1 | 2) bad=$bad,$n ;;
    *)
      echo "check: cmp failed to run (status $s)" >&2
      exit 2
      ;;
  esac
  rm -f "$C/want"
}

# 1: prorated fee 15/31 of a period; usage after END not billed.
new std
f accounts <<'E'
lark metered 2026-01-05 2026-03-20
E
f usage <<'E'
lark 2026-03-10 120
lark 2026-03-25 50
E
r 2026-03
w <<'E'
invoice lark 2026-03-05 2026-04-05
fee metered 15/31 2.42
usage 100 at 0.10 10.00
usage 20 at 0.08 1.60
total 14.02
E

# 2: grant usable on its last day (invoice date 2026-04-05 = DATE plus 25 days).
new std
f accounts <<'E'
mint basic 2026-03-05
E
f credits <<'E'
mint 2026-03-11 10.00 26
E
r 2026-03
w <<'E'
invoice mint 2026-03-05 2026-04-05
fee basic 30.00
credit 2026-03-11 -10.00
total 20.00
E

# 3: one-day grant dated on the invoice date.
new std
f accounts <<'E'
nori basic 2026-02-14
E
f credits <<'E'
nori 2026-03-14 12.50 1
E
r 2026-02
w <<'E'
invoice nori 2026-02-14 2026-03-14
fee basic 30.00
credit 2026-03-14 -12.50
total 17.50
E

# 4: anchor 31 after February.
new std
f accounts <<'E'
oak basic 2026-01-31
E
r 2026-04
w <<'E'
invoice oak 2026-04-30 2026-05-31
fee basic 30.00
total 30.00
E

# 5: trial end 2026-03-31; usage on 03-29 is in the February period and in the trial.
new std
f accounts <<'E'
pine trial 2026-01-31
E
f usage <<'E'
pine 2026-03-02 5
pine 2026-03-29 15
E
r 2026-02
w <<'E'
invoice pine 2026-02-28 2026-03-31
fee trial 10.00
total 10.00
E

# 6: anchor 30 through a leap February.
new std
f accounts <<'E'
quince metered 2027-12-30
E
f usage <<'E'
quince 2028-03-29 40
quince 2028-04-29 30
E
r 2028-03
w <<'E'
invoice quince 2028-03-30 2028-04-30
fee metered 5.00
usage 30 at 0.10 3.00
total 8.00
E

# 7: volume Q = 100 = first limit.
new std
f accounts <<'E'
ruby bulk 2026-03-01
E
f usage <<'E'
ruby 2026-03-12 100
E
r 2026-03
w <<'E'
invoice ruby 2026-03-01 2026-04-01
fee bulk 20.00
usage 100 at 0.10 10.00
total 30.00
E

# 8: volume Q = 1000 = second limit.
new std
f accounts <<'E'
rye bulk 2026-03-02
E
f usage <<'E'
rye 2026-03-05 400
rye 2026-03-20 600
E
r 2026-03
w <<'E'
invoice rye 2026-03-02 2026-04-02
fee bulk 20.00
usage 1000 at 0.08 80.00
total 100.00
E

# 9: volume Q = 250 = limit, with a discount after.
new
f plans <<'E'
vol2 15.00 volume 0
E
f tiers <<'E'
vol2 250 0.20
vol2 * 0.15
E
f accounts <<'E'
rush vol2 2026-03-04
E
f usage <<'E'
rush 2026-03-04 250
E
f discounts <<'E'
rush percent 10
E
r 2026-03
w <<'E'
invoice rush 2026-03-04 2026-04-04
fee vol2 15.00
usage 250 at 0.20 50.00
discount 10% -6.50
total 58.50
E

# 10: second account uses less than the first.
new std
f accounts <<'E'
sage bulk 2026-03-02
teak bulk 2026-03-02
E
f usage <<'E'
sage 2026-03-03 1500
teak 2026-03-04 50
E
r 2026-03
w <<'E'
invoice sage 2026-03-02 2026-04-02
fee bulk 20.00
usage 1500 at 0.05 75.00
total 95.00
invoice teak 2026-03-02 2026-04-02
fee bulk 20.00
usage 50 at 0.10 5.00
total 25.00
E

# 11: same account used more in its previous period.
new std
f accounts <<'E'
umber bulk 2026-02-10
E
f usage <<'E'
umber 2026-02-15 2000
umber 2026-03-15 300
E
r 2026-03
w <<'E'
invoice umber 2026-03-10 2026-04-10
fee bulk 20.00
usage 300 at 0.08 24.00
total 44.00
E

# 12: an earlier account's unprinted January invoice used more.
new std
f accounts <<'E'
vale bulk 2026-01-05
wren metered 2026-03-05
yew bulk 2026-03-05
E
f usage <<'E'
vale 2026-01-20 3000
wren 2026-03-06 400
yew 2026-03-06 40
E
r 2026-03
w <<'E'
invoice vale 2026-03-05 2026-04-05
fee bulk 20.00
total 20.00
invoice wren 2026-03-05 2026-04-05
fee metered 5.00
usage 100 at 0.10 10.00
usage 300 at 0.08 24.00
total 39.00
invoice yew 2026-03-05 2026-04-05
fee bulk 20.00
usage 40 at 0.10 4.00
total 24.00
E

# 13: fixed listed first.
new std
f accounts <<'E'
zinc basic 2026-03-01
E
f discounts <<'E'
zinc fixed 3.00
zinc percent 10
E
r 2026-03
w <<'E'
invoice zinc 2026-03-01 2026-04-01
fee basic 30.00
discount 10% -3.00
discount fixed -3.00
total 24.00
E

# 14: percent listed first.
new std
f accounts <<'E'
acorn metered 2026-03-03
E
f usage <<'E'
acorn 2026-03-10 200
E
f discounts <<'E'
acorn percent 20
acorn fixed 4.00
E
r 2026-03
w <<'E'
invoice acorn 2026-03-03 2026-04-03
fee metered 5.00
usage 100 at 0.10 10.00
usage 100 at 0.08 8.00
discount 20% -4.60
discount fixed -4.00
total 14.40
E

# 15: fixed discount capped after the percentage.
new std
f accounts <<'E'
basil basic 2026-03-08
E
f discounts <<'E'
basil fixed 25.00
basil percent 50
E
r 2026-03
w <<'E'
invoice basil 2026-03-08 2026-04-08
fee basic 30.00
discount 50% -15.00
discount fixed -15.00
total 0.00
E

# 16: older small grant first.
new std
f accounts <<'E'
cove basic 2026-03-10
E
f credits <<'E'
cove 2026-02-01 10.00 365
cove 2026-03-01 50.00 365
E
r 2026-03
w <<'E'
invoice cove 2026-03-10 2026-04-10
fee basic 30.00
credit 2026-02-01 -10.00
credit 2026-03-01 -20.00
total 0.00
E

# 17: file order newest first.
new std
f accounts <<'E'
dove basic 2026-03-10
E
f credits <<'E'
dove 2026-03-05 20.00 365
dove 2026-01-20 20.00 365
E
r 2026-03
w <<'E'
invoice dove 2026-03-10 2026-04-10
fee basic 30.00
credit 2026-01-20 -20.00
credit 2026-03-05 -10.00
total 0.00
E

# 18: balances left by the February invoice.
new std
f accounts <<'E'
elk basic 2026-02-10
E
f credits <<'E'
elk 2026-01-15 40.00 365
elk 2026-02-01 40.00 365
E
r 2026-03
w <<'E'
invoice elk 2026-03-10 2026-04-10
fee basic 30.00
credit 2026-01-15 -10.00
credit 2026-02-01 -20.00
total 0.00
E

# 19: two usage lines 0.125 and 0.105.
new
f plans <<'E'
micro 1.00 graduated 0
E
f tiers <<'E'
micro 10 0.0125
micro * 0.0105
E
f accounts <<'E'
fawn micro 2026-03-01
E
f usage <<'E'
fawn 2026-03-02 20
E
r 2026-03
w <<'E'
invoice fawn 2026-03-01 2026-04-01
fee micro 1.00
usage 10 at 0.0125 0.13
usage 10 at 0.0105 0.11
total 1.24
E

# 20: 10% of 12.35.
new
f plans <<'E'
studio 12.35 flat 0
E
f accounts <<'E'
fig studio 2026-03-04
E
f discounts <<'E'
fig percent 10
E
r 2026-03
w <<'E'
invoice fig 2026-03-04 2026-04-04
fee studio 12.35
discount 10% -1.24
total 11.11
E

# 21: three lines of 0.004.
new
f plans <<'E'
nano 2.00 graduated 0
E
f tiers <<'E'
nano 1 0.004
nano 2 0.004
nano * 0.004
E
f accounts <<'E'
gnat nano 2026-03-07
E
f usage <<'E'
gnat 2026-03-08 3
E
r 2026-03
w <<'E'
invoice gnat 2026-03-07 2026-04-07
fee nano 2.00
usage 1 at 0.004 0.00
usage 1 at 0.004 0.00
usage 1 at 0.004 0.00
total 2.00
E

# 22: usage on the end date.
new std
f accounts <<'E'
gale metered 2026-03-06
E
f usage <<'E'
gale 2026-03-20 50
gale 2026-04-06 30
E
r 2026-03
w <<'E'
invoice gale 2026-03-06 2026-04-06
fee metered 5.00
usage 50 at 0.10 5.00
total 10.00
E

# 23: calendar-month period; usage on both boundaries.
new std
f accounts <<'E'
hazel metered 2026-01-01
E
f usage <<'E'
hazel 2026-02-01 60
hazel 2026-02-27 40
hazel 2026-03-01 5
E
r 2026-02
w <<'E'
invoice hazel 2026-02-01 2026-03-01
fee metered 5.00
usage 100 at 0.10 10.00
total 15.00
E

# 24: usage on February's end date changes what the credit has left.
new std
f accounts <<'E'
iris metered 2026-02-06
E
f usage <<'E'
iris 2026-03-06 50
E
f credits <<'E'
iris 2026-02-06 12.00 365
E
r 2026-03
w <<'E'
invoice iris 2026-03-06 2026-04-06
fee metered 5.00
usage 50 at 0.10 5.00
credit 2026-02-06 -7.00
total 3.00
E

# 25: several accounts: file order, not started, ended, ascending volume.
new std
f accounts <<'E'
amber basic 2026-01-10
brook bulk 2026-03-01
coral metered 2026-04-01
dusk basic 2025-11-01 2026-02-01
ember bulk 2026-02-15
E
f usage <<'E'
brook 2026-03-09 500
ember 2026-03-20 2500
coral 2026-03-25 10
E
r 2026-03
w <<'E'
invoice amber 2026-03-10 2026-04-10
fee basic 30.00
total 30.00
invoice brook 2026-03-01 2026-04-01
fee bulk 20.00
usage 500 at 0.08 40.00
total 60.00
invoice ember 2026-03-15 2026-04-15
fee bulk 20.00
usage 2500 at 0.05 125.00
total 145.00
E

# 26: no usage; flat plan ignores usage.
new std
f accounts <<'E'
fjord metered 2026-03-09
glen basic 2026-03-09
E
f usage <<'E'
glen 2026-03-12 500
E
f discounts <<'E'
fjord percent 10
E
r 2026-03
w <<'E'
invoice fjord 2026-03-09 2026-04-09
fee metered 5.00
discount 10% -0.50
total 4.50
invoice glen 2026-03-09 2026-04-09
fee basic 30.00
total 30.00
E

# 27: exact half cents round up.
new
f plans <<'E'
half 1.00 graduated 0
hvol 0.00 volume 0
E
f tiers <<'E'
half * 0.0125
hvol * 0.0075
E
f accounts <<'E'
hob half 2026-03-02
hut hvol 2026-03-02
E
f usage <<'E'
hob 2026-03-03 2
hut 2026-03-03 6
E
r 2026-03
w <<'E'
invoice hob 2026-03-02 2026-04-02
fee half 1.00
usage 2 at 0.0125 0.03
total 1.03
invoice hut 2026-03-02 2026-04-02
fee hvol 0.00
usage 6 at 0.0075 0.05
total 0.05
E

# 28: empty output.
new std
f accounts <<'E'
# none of these has a February invoice

ivy basic 2026-03-01
  # an indented comment
jade	metered	2025-10-15	2026-01-15
E
f usage <<'E'
ivy 2026-03-02 10
E
r 2026-02
w </dev/null

# 29: 0.00 lines, no thousands separator, -0.50.
new
f plans <<'E'
free 0.00 flat 0
big 1234.50 flat 0
E
f accounts <<'E'
jolt free 2026-03-11
kelp big 2026-03-11
E
f discounts <<'E'
jolt percent 25
jolt percent 100
E
f credits <<'E'
kelp 2026-03-11 0.50 60
E
r 2026-03
w <<'E'
invoice jolt 2026-03-11 2026-04-11
fee free 0.00
discount 25% 0.00
discount 100% 0.00
total 0.00
invoice kelp 2026-03-11 2026-04-11
fee big 1234.50
credit 2026-03-11 -0.50
total 1234.00
E

# 30: usage on the start date is billed; trial ends on its end date.
new std
f accounts <<'E'
jet metered 2026-03-10
kiwi trial 2026-01-10
E
f usage <<'E'
jet 2026-03-09 40
jet 2026-03-10 25
kiwi 2026-03-10 12
E
r 2026-03
w <<'E'
invoice jet 2026-03-10 2026-04-10
fee metered 5.00
usage 25 at 0.10 2.50
total 7.50
invoice kiwi 2026-03-10 2026-04-10
fee trial 10.00
usage 10 at 0.20 2.00
usage 2 at 0.10 0.20
total 12.20
E

# 31: equal credit dates keep file order; percentages compound in file order.
new std
f accounts <<'E'
lime basic 2026-03-01
moss basic 2026-03-01
E
f discounts <<'E'
moss percent 50
moss percent 10
E
f credits <<'E'
lime 2026-02-01 10.00 365
lime 2026-02-01 50.00 365
E
r 2026-03
w <<'E'
invoice lime 2026-03-01 2026-04-01
fee basic 30.00
credit 2026-02-01 -10.00
credit 2026-02-01 -20.00
total 0.00
invoice moss 2026-03-01 2026-04-01
fee basic 30.00
discount 50% -15.00
discount 10% -1.50
total 13.50
E

# 32: a credit spread over invoices across a year end.
new std
f accounts <<'E'
nova metered 2025-11-20
E
f usage <<'E'
nova 2025-12-01 150
nova 2026-01-02 80
nova 2026-02-25 1200
E
f credits <<'E'
nova 2025-11-20 40.00 400
E
r 2026-02
w <<'E'
invoice nova 2026-02-20 2026-03-20
fee metered 5.00
usage 100 at 0.10 10.00
usage 900 at 0.08 72.00
usage 200 at 0.05 10.00
credit 2025-11-20 -3.00
total 94.00
E

[ "$n" = "$N" ] || exit 2
if [ -z "$bad" ]; then
  echo "cases $N of $N"
  exit 0
fi
echo "cases $pass of $N failed ${bad#,}"
exit 1
