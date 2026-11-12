cd /workspace || exit 2
LC_ALL=C
export LC_ALL
rm -rf /workspace/.check && mkdir /workspace/.check || exit 2
C=/workspace/.check
[ -f ledger/lib/ledger.awk ] && [ -f ledger/bin/ledger ] || exit 1
[ "$(find ledger/docs ledger/tests ledger/examples ledger/ISSUES.md | sort | tr '\n' ' ')" = "ledger/ISSUES.md ledger/docs ledger/docs/MANUAL.md ledger/examples ledger/examples/household.journal ledger/examples/unbalanced.journal ledger/tests ledger/tests/run.sh " ] || exit 1
[ "$(for f in $(find ledger/docs ledger/tests ledger/examples ledger/ISSUES.md -type f | sort); do sha256sum "$f" | cut -d ' ' -f 1; done | sha256sum | cut -d ' ' -f 1)" = "172090a229dceab2adb79854eb4a31af879ceaa0502b8890c50893d4fe5431a0" ] || exit 1
(cd / && sh /workspace/ledger/tests/run.sh) </dev/null >/dev/null 2>&1 || { echo "visible tests"; exit 1; }
n=0

# t STATUS ARGS...: run the ledger from / on ARGS. Stdout must equal
# $C/want, or be empty for a non-zero STATUS, and the exit status STATUS.
t() {
  n=$((n + 1))
  want=$1
  shift
  (cd / && sh /workspace/ledger/bin/ledger "$@") </dev/null >"$C/out" 2>"$C/err"
  rc=$?
  [ "$want" = 0 ] || : >"$C/want"
  if [ "$rc" != "$want" ] || ! cmp -s "$C/out" "$C/want"; then
    echo "case $n"
    exit 1
  fi
}

# e STATUS MESSAGE ARGS...: as t, with stderr exactly the line MESSAGE.
e() {
  st=$1
  printf '%s\n' "$2" >"$C/werr"
  shift 2
  t "$st" "$@"
  if ! cmp -s "$C/err" "$C/werr"; then
    echo "case $n"
    exit 1
  fi
}

# A prefix matches whole segments only.
printf '2026-05-01 Deposit\n    Assets:Bank  $100.00\n    Income:Gift\n2026-05-02 Escrow\n    Assets:Bankruptcy  $40.00\n    Income:Gift\n2026-05-03 Savings\n    Assets:Bank:Savings  $10.00\n    Assets:Bank\n2026-05-04 Banking fee\n    Expenses:Banking  $2.00\n    Assets:Bank\n' >"$C/j1"
cat >"$C/want" <<'END'
        $98.00  Assets
        $98.00    Bank
        $10.00      Savings
--------------
        $98.00
END
t 0 balance "$C/j1" Assets:Bank
cat >"$C/want" <<'END'
2026-05-01 Deposit              Assets:Bank                    $100.00       $100.00
2026-05-03 Savings              Assets:Bank:Savings             $10.00       $110.00
2026-05-03 Savings              Assets:Bank                    $-10.00       $100.00
2026-05-04 Banking fee          Assets:Bank                     $-2.00        $98.00
END
t 0 register "$C/j1" Assets:Bank
printf '%s\n' '--------------' '         $0.00' >"$C/want"
t 0 balance "$C/j1" Inc

# Amounts of a million dollars and more.
printf '2026-06-15 House purchase\n    Assets:House          $1,345,678.90\n    Liabilities:Mortgage  $-1,000,000.00\n    Assets:Bank:Checking\n2026-06-16 Bonus\n    Assets:Bank:Checking  $1,500,000.00\n    Income:Bonus\n' >"$C/j2"
cat >"$C/want" <<'END'
 $2,500,000.00  Assets
 $1,154,321.10    Bank
 $1,154,321.10      Checking
 $1,345,678.90    House
$-1,500,000.00  Income
$-1,500,000.00    Bonus
$-1,000,000.00  Liabilities
$-1,000,000.00    Mortgage
--------------
         $0.00
END
t 0 balance "$C/j2"
cat >"$C/want" <<'END'
2026-06-15 House purchase       Assets:House             $1,345,678.90 $1,345,678.90
2026-06-15 House purchase       Assets:Bank:Checking      $-345,678.90 $1,000,000.00
2026-06-16 Bonus                Assets:Bank:Checking     $1,500,000.00 $2,500,000.00
END
t 0 register "$C/j2" Assets

# An unbalanced transaction is reported at its header line.
printf '; Spring\n2026-04-01 Paint\n    Expenses:Paint   $14.99\n    ; a comment between postings\n    Assets:Cash  $-15.00\n\n2026-04-02 Never checked\n    Expenses:Paint   $1.00\n    Assets:Cash\n' >"$C/j3"
e 1 'unbalanced transaction at line 2' check "$C/j3"
printf '2026-04-01 Fine\n    Expenses:Paint   $1.00\n    Assets:Cash\n\n\n2026-04-03 One posting\n    Assets:Cash  $0.00\n' >"$C/j3"
e 1 'unbalanced transaction at line 6' check "$C/j3"
printf '2026-04-01 Fine\n    Expenses:Paint   $1.00\n    Assets:Cash\n2026-04-03 Two blanks\n    Expenses:Paint   $1.00\n    Assets:Cash\n    Assets:Bank\n2026-04-04 Later\ngarbage\n' >"$C/j3"
e 1 'unbalanced transaction at line 4' balance "$C/j3"

# Sums are exact, and zero is never negative.
printf '2026-07-01 Found\n    Assets:Cash   $0.30\n    Income:Misc\n2026-07-02 Snack\n    Expenses:Snacks  $0.10\n    Assets:Cash\n2026-07-03 Snack\n    Expenses:Snacks  $0.20\n    Assets:Cash\n' >"$C/j4"
cat >"$C/want" <<'END'
         $0.00  Assets
         $0.00    Cash
         $0.30  Expenses
         $0.30    Snacks
        $-0.30  Income
        $-0.30    Misc
--------------
         $0.00
END
t 0 balance "$C/j4"
cat >"$C/want" <<'END'
2026-07-01 Found                Assets:Cash                      $0.30         $0.30
2026-07-02 Snack                Assets:Cash                     $-0.10         $0.20
2026-07-03 Snack                Assets:Cash                     $-0.20         $0.00
END
t 0 register "$C/j4" Assets:Cash
printf '2026-12-05 Zero\n    Assets:Cash  $-0.00\n    Income:None  $0.00\n' >"$C/j10"
cat >"$C/want" <<'END'
         $0.00  Assets
         $0.00    Cash
         $0.00  Income
         $0.00    None
--------------
         $0.00
END
t 0 balance "$C/j10"

# register keeps file order for equal dates.
printf '2026-08-02 Market A\n    Expenses:Food  $5.00\n    Assets:Cash\n2026-08-02 Market B\n    Expenses:Food  $6.00\n    Assets:Cash\n2026-08-01 Market C\n    Expenses:Food  $7.00\n    Assets:Cash\n' >"$C/j5"
cat >"$C/want" <<'END'
2026-08-01 Market C             Expenses:Food                    $7.00         $7.00
2026-08-02 Market A             Expenses:Food                    $5.00        $12.00
2026-08-02 Market B             Expenses:Food                    $6.00        $18.00
END
t 0 register "$C/j5" Expenses
printf '2026-09-03 Split\n    Assets:Cash   $-1.00\n    Assets:Cash   $-2.00\n    Expenses:Misc\n2026-09-01 Earlier\n    Assets:Cash   $-3.00\n    Expenses:Misc\n' >"$C/j5"
cat >"$C/want" <<'END'
2026-09-01 Earlier              Assets:Cash                     $-3.00        $-3.00
2026-09-03 Split                Assets:Cash                     $-1.00        $-4.00
2026-09-03 Split                Assets:Cash                     $-2.00        $-6.00
END
t 0 register "$C/j5" Assets:Cash

# The elided amount when the others sum below zero.
printf '2026-10-01 Refund\n    Assets:Bank:Checking  $-20.00\n    Expenses:Returns  $-5.00\n    Income:Refunds\n' >"$C/j6"
cat >"$C/want" <<'END'
       $-20.00  Assets
       $-20.00    Bank
       $-20.00      Checking
        $-5.00  Expenses
        $-5.00    Returns
        $25.00  Income
        $25.00    Refunds
--------------
         $0.00
END
t 0 balance "$C/j6"

# Postings to a parent and its children; order by segment, bytewise.
printf '2026-11-01 Mixed\n    Expenses  $10.00\n    Expenses:Food  $5.00\n    Expenses:Car  $3.00\n    Expenses:Car Wash  $2.00\n    Expenses:Car:Fuel  $4.00\n    Assets:bank\n\n2026-11-02 Bank\n    Assets:Bank  $1.00\n    Assets:bank\n' >"$C/j7"
cat >"$C/want" <<'END'
       $-24.00  Assets
         $1.00    Bank
       $-25.00    bank
        $24.00  Expenses
         $7.00    Car
         $4.00      Fuel
         $2.00    Car Wash
         $5.00    Food
--------------
         $0.00
END
t 0 balance "$C/j7"

# Tabs, comments, long payees and accounts.
printf '; top comment\n2026-12-01   Tabbed payee   ; part of the payee\n\tExpenses:Eating Out\t$12.00 ; lunch\n    ; indented comment\n\tAssets:Cash   ; paid cash\n2026-12-02 A payee name longer than twenty\n    Expenses:Subscriptions:Streaming  $-1,234.50\n    Assets:Cash\n' >"$C/j9"
cat >"$C/want" <<'END'
2026-12-01 Tabbed payee   ; par Expenses:Eating Out             $12.00        $12.00
2026-12-02 A payee name longer  Expenses:Subscriptions:Streaming    $-1,234.50    $-1,222.50
END
t 0 register "$C/j9" Expenses
echo 'ok: 2 transactions' >"$C/want"
t 0 check "$C/j9"

# Syntax errors.
printf '2024-02-29 Leap day\n    Assets:Cash  $1.00\n    Income:Gift\n2000-02-29 Leap century\n    Assets:Cash  $1.00\n    Income:Gift\n1900-02-29 Not a leap century\n    Assets:Cash  $1.00\n    Income:Gift\n' >"$C/j8"
e 2 'syntax error at line 7' check "$C/j8"
printf '2026-04-30 Fine\n    Assets:Cash  $1.00\n    Income:Gift\n2026-04-31 No such day\n    Assets:Cash  $1.00\n    Income:Gift\n' >"$C/j8"
e 2 'syntax error at line 4' check "$C/j8"
printf '2026-01-01 A\n    Assets:Cash  $12.5\n    Income:Gift\n' >"$C/j8"
e 2 'syntax error at line 2' balance "$C/j8"
printf '2026-01-01 A\n    Assets:Cash  $1.00\n    Income:Gift  $-1,00.00\n' >"$C/j8"
e 2 'syntax error at line 3' check "$C/j8"
printf '2026-01-01 A\n    Assets:Cash  $1.00\n    Income:Gift\n\n    Assets:Cash  $1.00\n' >"$C/j8"
e 2 'syntax error at line 5' check "$C/j8"
printf '2026-01-01 A\n    Assets::Cash  $1.00\n    Income:Gift\n' >"$C/j8"
e 2 'syntax error at line 2' check "$C/j8"
printf '2026-01-01 A\n    Assets:Cash  $1.00\n    Income:Gift\n2026-01-02\n    Assets:Cash  $1.00\n    Income:Gift\n' >"$C/j8"
e 2 'syntax error at line 4' check "$C/j8"
printf '2026-01-01 A\n    Assets:Cash  $5.00\n    Income:Gift  $-4.00\n    Income:Gift  $x\n' >"$C/j8"
e 2 'syntax error at line 4' check "$C/j8"
printf '2026-01-01 A\n    Expenses:Eating  Out\n    Assets:Cash\n' >"$C/j8"
e 2 'syntax error at line 2' check "$C/j8"
printf '2026-01-01 A\n    Assets:Cash  12.00\n    Income:Gift\n' >"$C/j8"
e 2 'syntax error at line 2' register "$C/j8" Assets

# Command lines.
t 2 register "$C/j1"
t 2 balance "$C/j1" ''
t 2 tally "$C/j1"
t 2 check "$C/no-such.journal"
rm -rf /workspace/.check
