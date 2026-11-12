#!/bin/sh
# The ledger test suite. Run it as `sh tests/run.sh` from any directory.
cd "$(dirname "$0")/.." || exit 2
tmp=$(mktemp -d) || exit 2
trap 'rm -rf "$tmp"' EXIT
failures=0
n=0

# expect NAME STATUS ARGS...: run bin/ledger ARGS; its stdout must equal
# $tmp/want and its exit status STATUS.
expect() {
  name=$1
  status=$2
  shift 2
  n=$((n + 1))
  sh bin/ledger "$@" >"$tmp/out" 2>"$tmp/err"
  rc=$?
  if [ "$rc" = "$status" ] && cmp -s "$tmp/out" "$tmp/want"; then
    echo "ok $n $name"
  else
    echo "FAIL $n $name"
    failures=$((failures + 1))
  fi
}

echo 'ok: 9 transactions' >"$tmp/want"
expect "check a valid journal" 0 check examples/household.journal

cat >"$tmp/want" <<'END'
    $21,988.49  Assets
    $21,878.49    Bank
     $7,878.49      Checking
    $14,000.00      Savings
       $110.00    Cash
   $-15,350.00  Equity
   $-15,350.00    Opening
     $3,049.57  Expenses
        $63.40    Eating Out
        $86.17    Food
     $2,900.00    Rent
    $-9,624.66  Income
    $-9,624.66    Salary
       $-63.40  Liabilities
       $-63.40    Card
--------------
         $0.00
END
expect "balance of everything" 0 balance examples/household.journal

cat >"$tmp/want" <<'END'
     $3,049.57  Expenses
        $63.40    Eating Out
        $86.17    Food
     $2,900.00    Rent
--------------
     $3,049.57
END
expect "balance under a prefix" 0 balance examples/household.journal Expenses

cat >"$tmp/want" <<'END'
2026-01-01 Opening balances     Assets:Bank:Checking         $3,200.00     $3,200.00
2026-01-02 Landlord             Assets:Bank:Checking        $-1,450.00     $1,750.00
2026-01-05 Employer             Assets:Bank:Checking         $4,812.33     $6,562.33
2026-01-20 Card payment         Assets:Bank:Checking           $-46.17     $6,516.16
2026-02-02 Landlord             Assets:Bank:Checking        $-1,450.00     $5,066.16
2026-02-05 Employer             Assets:Bank:Checking         $4,812.33     $9,878.49
2026-02-28 Transfer to savings  Assets:Bank:Checking        $-2,000.00     $7,878.49
END
expect "register with a running total" 0 register examples/household.journal Assets:Bank:Checking

cat >"$tmp/want" <<'END'
2026-01-09 Corner market        Liabilities:Card               $-46.17       $-46.17
2026-01-20 Card payment         Liabilities:Card                $46.17         $0.00
2026-02-14 Bistro               Liabilities:Card               $-63.40       $-63.40
END
expect "register shows elided amounts" 0 register examples/household.journal Liabilities

: >"$tmp/want"
expect "check an unbalanced journal" 1 check examples/unbalanced.journal
n=$((n + 1))
if grep -q '^unbalanced transaction at line [0-9][0-9]*$' "$tmp/err"; then
  echo "ok $n unbalanced message"
else
  echo "FAIL $n unbalanced message"
  failures=$((failures + 1))
fi

printf '2026-01-01 Typo\n    Assets:Cash  $10.00\n    Income:Gift  $-10.0\n' >"$tmp/typo.journal"
expect "a malformed amount" 2 check "$tmp/typo.journal"
n=$((n + 1))
if [ "$(cat "$tmp/err")" = "syntax error at line 3" ]; then
  echo "ok $n syntax error message"
else
  echo "FAIL $n syntax error message"
  failures=$((failures + 1))
fi

expect "register needs a prefix" 2 register examples/household.journal

[ "$failures" = 0 ]
