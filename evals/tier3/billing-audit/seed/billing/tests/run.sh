#!/bin/sh
# The bill test suite. Run it as `sh tests/run.sh` from any directory.
# It exits 0 when every test passes.
cd "$(dirname "$0")/.." || exit 2
LC_ALL=C
export LC_ALL
tmp=$(mktemp -d) || exit 2
trap 'rm -rf "$tmp"' EXIT
failures=0
n=0

# expect NAME STATUS ARGS...: run bin/bill ARGS. Its standard output must
# equal $tmp/want, its standard error $tmp/werr, and its exit status STATUS.
expect() {
  name=$1
  status=$2
  shift 2
  n=$((n + 1))
  sh bin/bill "$@" >"$tmp/out" 2>"$tmp/err"
  rc=$?
  if [ "$rc" = "$status" ] && cmp -s "$tmp/out" "$tmp/want" && cmp -s "$tmp/err" "$tmp/werr"; then
    echo "ok $n $name"
  else
    echo "FAIL $n $name"
    failures=$((failures + 1))
  fi
}

# data FILE: write FILE of the test data directory $tmp/d from stdin.
data() {
  cat >"$tmp/d/$1"
}

# fresh: start a new, empty test data directory with the plans and tiers
# below.
fresh() {
  rm -rf "$tmp/d"
  mkdir "$tmp/d"
  data plans <<'END'
basic 30.00 flat 0
metered 5.00 graduated 0
bulk 20.00 volume 0
trial 10.00 graduated 1
END
  data tiers <<'END'
metered 100 0.10
metered 1000 0.08
metered * 0.05
bulk 500 0.04
bulk 5000 0.03
bulk * 0.02
trial 10 0.20
trial * 0.10
END
  : >"$tmp/want"
  : >"$tmp/werr"
}

# The examples.
: >"$tmp/werr"
cp examples/shop-2026-03.txt "$tmp/want"
expect "example shop" 0 examples/shop 2026-03
cp examples/studio-2026-05.txt "$tmp/want"
expect "example studio" 0 examples/studio 2026-05

# Command lines.
fresh
echo 'usage: bill DATADIR YYYY-MM' >"$tmp/werr"
expect "no arguments" 2
expect "a month that does not exist" 2 "$tmp/d" 2026-13
echo "bill: cannot read $tmp/d/accounts" >"$tmp/werr"
expect "no accounts file" 1 "$tmp/d" 2026-03

# A bad line.
fresh
data accounts <<'END'
acme metered 2026-01-15
END
data usage <<'END'
# ID DATE QUANTITY
acme 2026-03-20 5
acne 2026-03-21 5
END
echo 'bill: usage:3: bad line' >"$tmp/werr"
expect "usage of an unknown account" 1 "$tmp/d" 2026-03

# Graduated usage over three tiers.
fresh
data accounts <<'END'
acme metered 2026-01-15
END
data usage <<'END'
acme 2026-03-15 600
acme 2026-04-01 1000
acme 2026-04-14 400
END
cat >"$tmp/want" <<'END'
invoice acme 2026-03-15 2026-04-15
fee metered 5.00
usage 100 at 0.10 10.00
usage 900 at 0.08 72.00
usage 1000 at 0.05 50.00
total 137.00
END
expect "graduated over three tiers" 0 "$tmp/d" 2026-03

# Volume usage: every unit at one tier's price.
fresh
data accounts <<'END'
birch bulk 2026-02-03
END
data usage <<'END'
birch 2026-03-04 650
birch 2026-03-30 600
END
cat >"$tmp/want" <<'END'
invoice birch 2026-03-03 2026-04-03
fee bulk 20.00
usage 1250 at 0.03 37.50
total 57.50
END
expect "volume in the middle tier" 0 "$tmp/d" 2026-03

# A fixed discount never takes more than remains.
fresh
data accounts <<'END'
cedar basic 2026-02-10
END
data discounts <<'END'
cedar fixed 45.00
END
cat >"$tmp/want" <<'END'
invoice cedar 2026-03-10 2026-04-10
fee basic 30.00
discount fixed -30.00
total 0.00
END
expect "fixed discount larger than the invoice" 0 "$tmp/d" 2026-03

# Usage in the trial is free.
fresh
data accounts <<'END'
dune trial 2026-02-12
END
data usage <<'END'
dune 2026-02-20 50
dune 2026-03-11 7
dune 2026-03-13 30
END
cat >"$tmp/want" <<'END'
invoice dune 2026-02-12 2026-03-12
fee trial 10.00
total 10.00
END
expect "trial usage is free" 0 "$tmp/d" 2026-02
cat >"$tmp/want" <<'END'
invoice dune 2026-03-12 2026-04-12
fee trial 10.00
usage 10 at 0.20 2.00
usage 20 at 0.10 2.00
total 14.00
END
expect "usage after the trial" 0 "$tmp/d" 2026-03

# A credit lasts over several invoices.
fresh
data accounts <<'END'
elm basic 2026-01-20
END
data credits <<'END'
elm 2026-01-25 50.00 120
END
cat >"$tmp/want" <<'END'
invoice elm 2026-03-20 2026-04-20
fee basic 30.00
total 30.00
END
expect "credit used up by earlier invoices" 0 "$tmp/d" 2026-03
cat >"$tmp/want" <<'END'
invoice elm 2026-02-20 2026-03-20
fee basic 30.00
credit 2026-01-25 -20.00
total 10.00
END
expect "the rest of a credit" 0 "$tmp/d" 2026-02

# Accounts that have not started have no invoice.
fresh
data accounts <<'END'
fern basic 2026-05-01
END
expect "no invoices" 0 "$tmp/d" 2026-04

if [ "$failures" -ne 0 ]; then
  echo "$failures of $n tests failed"
  exit 1
fi
echo "all $n tests passed"
