# ledger manual

`ledger` keeps accounts in a plain-text journal using double-entry
bookkeeping: every transaction moves money between accounts, and its
amounts add up to zero. This manual is the specification. Where the
program and the manual disagree, the program is wrong.

```text
ledger check FILE
ledger balance FILE [PREFIX]
ledger register FILE PREFIX
```

## 1. The journal

A journal is a text file of LF-separated lines. Trailing spaces and tabs
on any line are ignored. "Blanks" below are spaces and tabs.

Each line is one of:

- **Blank:** empty, or only blanks. A blank line ends the transaction
  before it, if any.
- **Comment:** optional blanks, then `;`, then anything. Comment lines are
  ignored wherever they appear, including between the postings of a
  transaction; they do not end it.
- **Transaction header:** a date `YYYY-MM-DD` at the very start of the
  line, then one or more blanks, then the payee, which is the rest of the
  line and is not empty. The date must be a real day of the Gregorian
  calendar: `2024-02-29` is valid, `2026-02-29` and `2026-04-31` are not.
  A header starts a new transaction and ends the one before it, if any.
- **Posting:** one or more blanks, then an account, then optionally a
  separator and an amount. A posting belongs to the transaction whose
  header came last; a posting with no transaction before it (at the start
  of the file or after a blank line) is a syntax error.

Any other line is a syntax error.

### 1.1 Postings

In a posting line, a `;` that follows a blank starts a comment that runs
to the end of the line, and is removed before anything else. Then:

- The **separator** is the first tab, or the first run of two spaces, after
  the leading blanks. Everything before it is the account; everything after
  it, with leading blanks removed, is the amount.
- With no separator, the whole rest of the line is the account and the
  amount is left out (see 1.3).
- If the separator is followed by nothing, the amount is left out too.

**Accounts** are one or more segments separated by `:`, such as
`Assets:Bank:Checking`. A segment is not empty and does not start or end
with a space; it may contain single spaces, as in `Expenses:Eating Out`.
Accounts are case-sensitive. An account is *below* another when it starts
with that account's whole segments: `Assets:Bank:Checking` is below
`Assets:Bank` and `Assets`. Every prefix of an account's segments is an
account too, even if nothing is posted to it directly.

### 1.2 Amounts

An amount is written `$`, then an optional `-`, then the whole dollars,
then `.` and exactly two digits of cents. The whole dollars are either
plain digits (`1234`) or groups of three digits separated by `,` with one
to three digits before the first `,` (`1,234` and `12,345,678`). So
`$1,234.50`, `$1234.50`, `$-0.75` and `$0.00` are amounts, and `$12.5`,
`$1,23.00`, `12.00` and `$+1.00` are syntax errors. `$-0.00` is zero.

Amounts are exact to the cent. Every amount, and every running or total
sum, stays within $-10,000,000,000,000.00 to $10,000,000,000,000.00
(10^15 cents either way); beyond that the behaviour is unspecified. Within
that range arithmetic is exact: no rounding ever happens.

The program prints amounts the same way, always with the thousands
separators: `$1,234.50`, `$-1,234,567.00`, `$0.05`. A `-` is printed only
for an amount below zero; zero is always `$0.00`.

### 1.3 Balancing

A transaction must have at least two postings. At most one of them may
leave its amount out; that posting gets the amount that makes the
transaction's amounts add up to exactly zero. Otherwise the amounts must
already add up to exactly zero.

For example, in

```text
2026-03-02 Grocer
    Expenses:Food       $40.00
    Expenses:Household  $12.50
    Assets:Bank:Checking
```

the last posting's amount is `$-52.50`.

### 1.4 Errors

The journal is read from top to bottom, and the first problem found is
reported on standard error, as exactly one line:

- `syntax error at line N`, exit status 2, where N is the line that is not
  valid;
- `unbalanced transaction at line N`, exit status 1, for a transaction that
  does not balance as described in 1.3 (too few postings, more than one
  posting without an amount, or amounts that do not add up to zero). N is
  the line of the transaction's **header**, wherever the problem is in the
  transaction.

A transaction is checked when it ends: at the next blank line, the next
header, or the end of the file. So a syntax error inside a transaction is
reported before that transaction's balance is checked, and an unbalanced
transaction is reported before any syntax error after it.

Every command checks the whole journal first, and does nothing else if it
finds a problem. A FILE that cannot be read, or a command line that does
not match the forms at the top of this manual, also exits with status 2.
`PREFIX`, when given, must not be empty.

## 2. Selecting accounts

`balance` and `register` take a PREFIX, which is an account. A posting is
**selected** when its account is PREFIX or below PREFIX, matching whole
segments only: the PREFIX `Assets:Bank` selects `Assets:Bank` and
`Assets:Bank:Savings`, but not `Assets:Bankruptcy` or `Assets:Banking`.
Without a PREFIX, `balance` selects every posting.

## 3. Commands

### 3.1 check

`ledger check FILE` checks the journal and prints `ok: N transactions`,
where N is the number of transactions.

### 3.2 balance

`ledger balance FILE [PREFIX]` prints the tree of accounts with the total
of the selected postings for each.

The tree holds every account of a selected posting, and every account
above it. An account's total is the sum of the selected postings to that
account itself and to every account below it. (A posting is counted once
in each account it belongs to: a posting to `Expenses:Food` counts in
`Expenses:Food` and in `Expenses`, and a posting directly to `Expenses`
counts in `Expenses` only.)

Each account is printed as one line: its total right-aligned in a field of
14 characters, two spaces, two more spaces for each level below the top,
then the account's last segment. The tree is printed depth first: each
account is followed by the accounts directly below it, each of those by
the accounts below it, and so on, before the next account at its own
level. Accounts directly below the same account (or at the top level) are
ordered by their last segment, comparing bytes, so `Bank` comes before
`Bank Fees`, which comes before `bank`.

After the tree come a line of 14 `-` characters and the sum of all
selected postings, right-aligned in 14 characters. With no selected
postings, only those two lines are printed, the sum being `$0.00`.

For example, for the journal

```text
2026-03-01 Opening balance
    Assets:Bank:Checking  $2,500.00
    Equity:Opening

2026-03-02 Grocer
    Expenses:Food       $40.00
    Expenses:Household  $12.50
    Assets:Bank:Checking
```

`ledger balance FILE` prints

```text
     $2,447.50  Assets
     $2,447.50    Bank
     $2,447.50      Checking
    $-2,500.00  Equity
    $-2,500.00    Opening
        $52.50  Expenses
        $40.00    Food
        $12.50    Household
--------------
         $0.00
```

and `ledger balance FILE Expenses` prints

```text
        $52.50  Expenses
        $40.00    Food
        $12.50    Household
--------------
        $52.50
```

### 3.3 register

`ledger register FILE PREFIX` prints the selected postings, one per line,
with a running total.

The postings are in date order. Postings with the same date stay in the
order they have in the file, whether they are in the same transaction or
not. Journals need not be written in date order.

Each line is: the date, a space, the payee cut to its first 20 characters
and padded with spaces to 20, a space, the account padded with spaces to
24 characters (a longer account is printed in full), a space, the
posting's amount right-aligned in 13 characters, a space, and the running
total, right-aligned in 13 characters. The running total is the sum of the
amounts on this line and all the lines before it. A left-out amount is
shown as the amount it gets (1.3).

For the journal in 3.2, `ledger register FILE Assets` prints

```text
2026-03-01 Opening balance      Assets:Bank:Checking         $2,500.00     $2,500.00
2026-03-02 Grocer               Assets:Bank:Checking           $-52.50     $2,447.50
```
