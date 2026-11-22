# bill: manual

`bill` prints the monthly invoices of a small subscription business. It
reads plain-text data files that describe plans, accounts, usage,
discounts and credits, and prints one invoice per account for the month
it is asked about.

This manual is the specification of `bill`. Where the program and the
manual disagree, the manual is right.

## 1. Conventions

These rules apply everywhere in `bill`. The later sections rely on them
and do not repeat them.

**1.1 Dates.** A date is written `YYYY-MM-DD` and must be a real day of
the Gregorian calendar: `2028-02-29` is a date, `2026-02-29` is not.
"D plus N days" is the day N days after D: 2026-03-01 plus 30 days is
2026-03-31.

**1.2 Ranges of days.** Every range of days in this manual is half-open.
The range from date A to date B contains A and every day after it up to,
but not including, B. The range from 2026-03-05 to 2026-04-05 contains
2026-03-05 and 2026-04-04, but not 2026-04-05. "The number of days from A
to B" is the number of days in that range: from 2026-03-05 to 2026-04-05
is 31 days, and from 2026-03-05 to 2026-03-06 is 1 day.

**1.3 Rounding.** An invoice is a list of line items. Amounts are worked
out exactly, with no rounding along the way; only line items are
rounded. Each line item is rounded to whole cents, half away from zero,
before it is added to anything: 0.125 becomes 0.13, -1.235
becomes -1.24, and 0.004 becomes 0.00. Every sum on an invoice is a sum of
rounded line items, so an invoice's total is always exactly the sum of
the amounts printed above it.

**1.4 Printing money.** Amounts of money are printed with a `-` in front
if they are negative, at least one digit before the decimal point, no
leading zeros other than a single `0` before the point, no thousands
separators, and exactly two digits after it: `1234.50`, `-0.05`,
`0.00`. Zero is always printed `0.00`, never `-0.00`.

**1.5 Order.** When this manual puts things in order and two of them
are equal, they keep the order of their lines in the data files.

## 2. Running bill

    bill DATADIR PERIOD

DATADIR is a directory of data files (section 3). PERIOD is a month,
written `YYYY-MM`, with a year from 0001 and a month from 01 to 12.

`bill` prints the invoice for PERIOD (section 4) of every account that
has one, in the order of the accounts file, and exits with status 0. If
no account has an invoice for PERIOD, it prints nothing and exits with
status 0.

Errors. `bill` prints nothing on standard output when it reports an
error; the message goes to standard error.

- With the wrong number of arguments or a PERIOD that is not a month
  written as above, it prints `usage: bill DATADIR YYYY-MM` and exits
  with status 2.
- If DATADIR has no readable file `plans` or no readable file
  `accounts`, it prints `bill: cannot read DATADIR/plans` (or
  `DATADIR/accounts`), with DATADIR as it was given, and exits with
  status 1. `plans` is checked first.
- If a data file has a bad line (section 3), it prints
  `bill: FILE:N: bad line`, where FILE is the file's name (`plans`,
  `usage`, ...) and N the line's number in that file, and exits with
  status 1. Only the first bad line is reported, taking the files in
  the order plans, tiers, accounts, usage, discounts, credits.

## 3. Data files

DATADIR may hold six files: `plans`, `tiers`, `accounts`, `usage`,
`discounts` and `credits`. `plans` and `accounts` are required; any
other file that is missing counts as empty.

Every file has one record per line, with fields separated by spaces or
tabs. Empty lines, lines of only spaces and tabs, and lines whose first
character other than a space or tab is `#` are ignored. Any other line
is bad if it has the wrong number of fields or a field that is not as
described below.

The kinds of field:

- NAME: one or more letters, digits and `-`.
- DATE: a date (1.1).
- MONEY: digits, a point and two digits, as in `30.00`.
- PRICE: digits, optionally followed by a point and one to four digits,
  as in `2`, `0.08` or `0.0125`.
- COUNT: a whole number above zero, written without leading zeros.

**plans**: `NAME FEE MODEL TRIAL`. One line per plan. NAME must not be
used by an earlier plan. FEE is the monthly fee, as MONEY. MODEL is
`flat`, `graduated` or `volume` (section 6). TRIAL is a whole number of
months, `0` for none, written with digits only (section 6).

**tiers**: `PLAN LIMIT PRICE`. The usage tiers of graduated and volume
plans (section 6), each plan's tiers in order. PLAN must be defined in
`plans`. LIMIT is a COUNT greater than the LIMIT of the plan's previous
tier, or `*` for no limit; no tier may follow a plan's `*` tier. PRICE
is the price of one unit. Each graduated or volume plan has at least
one tier, and its last tier has LIMIT `*`. Tiers of a flat plan are
allowed and not used.

**accounts**: `ID PLAN START` or `ID PLAN START END`. ID is a NAME that
must not be used by an earlier account. PLAN must be defined in `plans`.
START is the DATE the account starts. END, if present, is a DATE after
START: the account is active from START to END (1.2), and END is the
first day it is not active.

**usage**: `ID DATE QUANTITY`. Account ID, defined in `accounts`, used
QUANTITY (a COUNT) units on DATE.

**discounts**: `ID percent N` or `ID fixed AMOUNT`. Account ID, defined
in `accounts`, has a discount of N percent, where N is a whole number
from 1 to 100 written without leading zeros, or of AMOUNT (MONEY). A
discount applies to every invoice of its account (section 7).

**credits**: `ID DATE AMOUNT DAYS`. Account ID, defined in `accounts`,
was granted AMOUNT (MONEY) of credit on DATE, usable for DAYS (a COUNT)
days (section 8).

## 4. Billing periods

Each account is billed monthly from its START date. Its periods are
numbered from 0: period k runs from START plus k months to START plus
k + 1 months. So period 0 runs from START to START plus one month, and
each period ends on the day the next one starts.

**Adding months.** D plus k months is the date in the k-th month after
D's month that has D's day of the month or, if that month is too short,
its last day. The day of the month always comes from D itself, so the
periods of an account started on 2026-01-31 start on 2026-01-31,
2026-02-28, 2026-03-31, 2026-04-30, 2026-05-31, and so on, and every
period of an account started on 2026-03-15 starts on the 15th.

**Which invoice.** An account's invoice for PERIOD is for its period
that starts in the month PERIOD. An account that starts after the month
PERIOD has no invoice for it.

**Ending.** An account with an END date has no invoice for any period
that starts on or after END. If END is inside a period (after its start
and before its end), that period is the account's last, and its fee is
prorated (section 5).

**The invoice date.** Every invoice is dated the day its period ends,
which is the day the next period starts and is not itself in the
period, whether or not the account ended earlier.

**Earlier invoices.** A credit used on one invoice can't be used again
on a later one (section 8), so to print an account's invoice for PERIOD,
`bill` works out every invoice of the account from period 0 on, in
order, exactly as this manual describes, and prints only the one for
PERIOD.

## 5. The fee

Every invoice charges the plan's FEE as its first line item.

When the account's END is inside the period, the fee is prorated
instead: the line item is FEE times the number of days from the
period's start to END, divided by the number of days from the period's
start to its end. For example, a 30.00 fee for a period from 2026-04-10
to 2026-05-10, with END 2026-04-25, is 30.00 x 15 / 30 = 15.00.

## 6. Usage

**Billed usage.** An invoice bills the account's usage lines whose DATE
is in its period, or, when END is inside the period, in the range from
the period's start to END. The invoice's quantity Q is the sum of the
QUANTITY of those lines, leaving out any line whose DATE is before the
end of the account's trial. Q is the account's own quantity on this one
invoice: other accounts' usage, and the account's usage on other
invoices, never change what this invoice charges.

**Trial.** An account whose plan has a TRIAL of T months has a trial
from START to START plus T months (section 4). Usage during the trial is
free. The fee is charged as usual.

**Models.** A `flat` plan charges nothing for usage, and its invoices
have no usage lines. A `graduated` or `volume` plan charges Q by its
tiers. When Q is 0 the invoice has no usage lines.

**Tiers.** A plan's first tier holds the quantities from 1 up to and
including its LIMIT; each later tier holds the quantities above the
previous tier's LIMIT, up to and including its own. A LIMIT is always
inside its own tier: with limits `100` and `*`, a quantity of 100 is in
the first tier and 101 in the second. A tier with LIMIT `*` has no upper
end.

**Graduated.** The units are spread over the tiers in order: the first
tier gets as many of them as it holds, the next tier as many of the
rest as it holds, and so on. Each tier that gets at least one unit is
one line item: N units times the tier's PRICE. With tiers `100 0.10`,
`1000 0.08` and `* 0.05`, a quantity of 250 is 100 units at 0.10 and 150
at 0.08.

**Volume.** All Q units are charged at the PRICE of the one tier that
holds Q, as one line item. With the same tiers, a quantity of 250 is 250
units at 0.08.

**Usage lines.** A usage line item is printed as
`usage N at PRICE AMOUNT`, with N the number of units, PRICE exactly as
written in the tiers file, and AMOUNT the rounded line item.

## 7. Discounts

After the fee and the usage lines, each of the account's discounts is a
line item: first every percentage discount, in the order of the
discounts file, then every fixed discount, in the order of the
discounts file. Each discount takes from the amount remaining, which is
the sum of the invoice's line items before it:

- a percentage discount of N takes N percent of the amount remaining;
- a fixed discount takes its AMOUNT, or the whole amount remaining if
  that is less.

The line item is minus what the discount takes. A discount's line is
printed even if it takes nothing.

For example, with a fee of 100.00 and the discounts `fixed 5.00` and
`percent 10`, in that order in the file, the 10% discount takes 10.00
and then the fixed discount takes 5.00, leaving 85.00.

## 8. Credits

A credits line gives the account AMOUNT of credit, usable from DATE to
DATE plus DAYS days. An invoice can use the grant if its invoice date
(section 4) is in that range. A grant of 10 days on 2026-03-01 can be
used by invoices dated 2026-03-01 to 2026-03-10, but not by one dated
2026-03-11.

After the discounts, the invoice uses the grants it can use, oldest
first, by their DATE. Each grant in turn takes as much of its balance as
it can, but no more than the amount remaining on the invoice; once
nothing remains, no further grant is used. A grant's balance is its
AMOUNT less what the account's earlier invoices took from it. Each grant
that takes more than zero is a line item, minus what it took.

## 9. The invoice

An invoice is printed as these lines, in this order, each of them words
separated by single spaces:

    invoice ID FROM TO
    fee PLAN AMOUNT
    usage N at PRICE AMOUNT
    discount N% AMOUNT
    discount fixed AMOUNT
    credit DATE AMOUNT
    total AMOUNT

- `invoice`: the account's ID, then the start (FROM) and end (TO) of
  the period.
- `fee`: the plan's NAME and the fee line item. A prorated fee is
  printed `fee PLAN D/P AMOUNT`, where D is the number of days from the
  period's start to END and P the number of days in the period.
- `usage`: zero or more lines (section 6), in tier order.
- `discount`: one line for each of the account's discounts, in the order
  they apply (section 7), N being the percentage.
- `credit`: one line for each grant used, in the order used, with the
  grant's DATE.
- `total`: what remains after the credits: the sum of all the line items
  above.

AMOUNT is always the line item's rounded amount, printed as in 1.4.
Invoices follow each other directly, with no empty line between them.

Example. With the plan `metered 5.00 graduated 0`, its tiers
`metered 100 0.10`, `metered 1000 0.08` and `metered * 0.05`, the
account `acme metered 2026-01-15`, 120 units of usage on 2026-03-20 and
the discount `acme percent 10`, `bill DATADIR 2026-03` prints:

    invoice acme 2026-03-15 2026-04-15
    fee metered 5.00
    usage 100 at 0.10 10.00
    usage 20 at 0.08 1.60
    discount 10% -1.66
    total 14.94
