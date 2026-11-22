cd /workspace || exit 2
cat > billing/lib/dates.awk <<'BILL_EOF'
# dates.awk: calendar arithmetic for bill.
#
# A date is a string YYYY-MM-DD (docs/MANUAL.md, section 1). Dates in
# that form compare correctly as strings, so the rest of bill compares
# them with < and >= directly. This file holds everything else: checking
# that a string is a real date, counting days, and adding months.

# leap(y): 1 if y is a leap year of the Gregorian calendar.
function leap(y) {
  return (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

# month_days(y, m): the number of days in month m (1 to 12) of year y.
function month_days(y, m) {
  if (m == 2)
    return leap(y) ? 29 : 28
  if (m == 4 || m == 6 || m == 9 || m == 11)
    return 30
  return 31
}

# date_y(s), date_m(s), date_d(s): the parts of date s, as numbers.
function date_y(s) { return substr(s, 1, 4) + 0 }
function date_m(s) { return substr(s, 6, 2) + 0 }
function date_d(s) { return substr(s, 9, 2) + 0 }

# make_date(y, m, d): the date string for year y, month m, day d.
function make_date(y, m, d) {
  return sprintf("%04d-%02d-%02d", y, m, d)
}

# is_date(s): 1 if s is written YYYY-MM-DD and is a real day.
function is_date(s,    y, m, d) {
  if (s !~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/)
    return 0
  y = date_y(s)
  m = date_m(s)
  d = date_d(s)
  return y >= 1 && m >= 1 && m <= 12 && d >= 1 && d <= month_days(y, m)
}

# month_index(s): a running number for the month of date or month s, so
# that consecutive months get consecutive numbers.
function month_index(s) {
  return (substr(s, 1, 4) + 0) * 12 + substr(s, 6, 2) - 1
}

# day_number(s): a running number for date s, so that consecutive days
# get consecutive numbers. Day 1 is 0001-01-01.
function day_number(s,    y, m, d, n, i) {
  y = date_y(s)
  m = date_m(s)
  d = date_d(s)
  # whole years before y, with their leap days
  n = (y - 1) * 365 + int((y - 1) / 4) - int((y - 1) / 100) + int((y - 1) / 400)
  # whole months before m in year y
  for (i = 1; i < m; i++)
    n += month_days(y, i)
  return n + d
}

# days_between(a, b): the number of days from date a to date b.
function days_between(a, b) {
  return day_number(b) - day_number(a)
}

# add_months(s, k): date s plus k months, k >= 0 (MANUAL section 4):
# the same day of the month k months later, or the last day of that
# month when it is shorter.
function add_months(s, k,    y, m, d, i) {
  y = date_y(s)
  m = date_m(s)
  d = date_d(s)
  m += k
  y += int((m - 1) / 12)
  m = (m - 1) % 12 + 1
  if (d > month_days(y, m))
    d = month_days(y, m)
  return make_date(y, m, d)
}
BILL_EOF
cat > billing/lib/tiers.awk <<'BILL_EOF'
# tiers.awk: usage charges for graduated and volume plans.
#
# A plan's tiers come from the tiers file, in order: tier t of plan p
# has the limit tlimit[p, t] (-1 for "*", no limit) and the unit price
# tprice[p, t] (as written) or tunits[p, t] (in units). ntier[p] is how
# many tiers the plan has. MANUAL section 6 describes both models.

# within(p, t, q): 1 if quantity q is inside the limit of plan p's tier t.
function within(p, t, q) {
  return tlimit[p, t] < 0 || q <= tlimit[p, t]
}

# graduated(p, q): charge q units on graduated plan p. The units fill
# the tiers in order; each tier that gets any units is one line item.
function graduated(p, q,    t, lo, n) {
  lo = 0
  for (t = 1; t <= ntier[p] && q > lo; t++) {
    if (within(p, t, q))
      n = q - lo
    else
      n = tlimit[p, t] - lo
    item("usage " n " at " tprice[p, t], n * tunits[p, t])
    lo += n
  }
}

# volume(p, q): charge q units on volume plan p. All of them cost the
# price of the one tier whose range holds q: one line item.
function volume(p, q,    t) {
  t = 1
  while (t < ntier[p] && !within(p, t, q))
    t++
  item("usage " q " at " tprice[p, t], q * tunits[p, t])
}
BILL_EOF
cat > billing/lib/discounts.awk <<'BILL_EOF'
# discounts.awk: an account's discounts (MANUAL section 7).
#
# Discount i belongs to account dacct[i]. Its kind dkind[i] is
# "percent", with dvalue[i] the whole percentage, or "fixed", with
# dvalue[i] the amount in units. ndisc is how many there are, in file
# order. Every discount applies to every invoice of its account.

# percent_off(i): apply percentage discount i to the current invoice.
# A percentage of a whole number of cents is a whole number of units.
function percent_off(i) {
  item("discount " dvalue[i] "%", -remaining() * dvalue[i])
}

# fixed_off(i): apply fixed discount i to the current invoice. It takes
# its amount, or what remains of the invoice if that is less.
function fixed_off(i,    take) {
  take = dvalue[i]
  if (take > remaining() * 100)
    take = remaining() * 100
  item("discount fixed", -take)
}

# apply_discounts(a): apply account a's discounts to the current invoice.
function apply_discounts(a,    i) {
  for (i = 1; i <= ndisc; i++)
    if (dacct[i] == a && dkind[i] == "percent")
      percent_off(i)
  for (i = 1; i <= ndisc; i++)
    if (dacct[i] == a && dkind[i] == "fixed")
      fixed_off(i)
}
BILL_EOF
cat > billing/lib/credits.awk <<'BILL_EOF'
# credits.awk: credit grants (MANUAL section 8).
#
# Grant g belongs to account gacct[g]. It was granted on gdate[g], for
# gdays[g] days, and gleft[g] is the part of it, in cents, that earlier
# invoices have not used yet. ngrant is how many grants there are, in
# file order.

# usable(g, d): 1 if grant g can be used by an invoice dated d.
function usable(g, d) {
  if (gdate[g] > d)
    return 0
  return days_between(gdate[g], d) < gdays[g]
}

# apply_credits(a, d): use account a's grants on the current invoice,
# which is dated d, until they or the invoice run out.
function apply_credits(a, d,    g, n, j, i, list, take) {
  # list[1..n]: the usable grants, sorted by date
  n = 0
  for (g = 1; g <= ngrant; g++) {
    if (gacct[g] != a || gleft[g] == 0 || !usable(g, d))
      continue
    for (j = n; j >= 1 && gdate[list[j]] > gdate[g]; j--)
      list[j + 1] = list[j]
    list[j + 1] = g
    n++
  }
  for (i = 1; i <= n && remaining() > 0; i++) {
    g = list[i]
    take = gleft[g]
    if (take > remaining())
      take = remaining()
    gleft[g] -= take
    item("credit " gdate[g], -take * 100)
  }
}
BILL_EOF
cat > billing/lib/main.awk <<'BILL_EOF'
# main.awk: reads the data files and prints the invoices.
#
# bin/bill runs every module together as one awk program:
#
#   awk -v period=PERIOD -f dates.awk -f money.awk ... -f main.awk FILES
#
# where FILES are the data files that exist, in the order plans, tiers,
# accounts, usage, discounts, credits. Each line is checked as it is
# read; the first bad one stops bill (MANUAL section 2). The invoices
# are worked out in END, once everything has been read.
#
# What is read:
#
#   plans       pfee[p] (units), pmodel[p], ptrial[p] (months)
#   tiers       ntier[p], tlimit[p, t], tprice[p, t], tunits[p, t]
#   accounts    acct[1..nacct] (file order), aplan[a], astart[a], aend[a]
#   usage       uacct[i], udate[i], uqty[i] for i = 1..nuse
#   discounts   dacct[i], dkind[i], dvalue[i] for i = 1..ndisc
#   credits     gacct[g], gdate[g], gleft[g] (cents), gdays[g], g = 1..ngrant

BEGIN {
  failed = 0
  nacct = nuse = ndisc = ngrant = 0
}

# bad(): report the current line as bad and stop.
function bad() {
  if (failed)
    return
  print "bill: " file ":" FNR ": bad line" | "cat 1>&2"
  close("cat 1>&2")
  failed = 1
  exit 1
}

# is_name(s): 1 if s is a plan name or account ID.
function is_name(s) {
  return s ~ /^[A-Za-z0-9-]+$/
}

# is_count(s): 1 if s is a whole number greater than zero.
function is_count(s) {
  return s ~ /^[1-9][0-9]*$/
}

FNR == 1 {
  file = FILENAME
  sub(/.*\//, "", file)
}

/^[ \t]*(#|$)/ {
  next
}

file == "plans" {
  if (NF != 4 || !is_name($1) || ($1 in pmodel) || !is_money($2))
    bad()
  if ($3 != "flat" && $3 != "graduated" && $3 != "volume")
    bad()
  if ($4 !~ /^[0-9]+$/)
    bad()
  pfee[$1] = units($2)
  pmodel[$1] = $3
  ptrial[$1] = $4 + 0
  ntier[$1] = 0
  next
}

file == "tiers" {
  if (NF != 3 || !($1 in pmodel) || !is_price($3))
    bad()
  p = $1
  t = ntier[p]
  # a limit must be above the one before it, and nothing follows "*"
  if (t > 0 && tlimit[p, t] < 0)
    bad()
  if ($2 == "*")
    limit = -1
  else if (is_count($2) && (t == 0 || $2 + 0 > tlimit[p, t]))
    limit = $2 + 0
  else
    bad()
  ntier[p] = ++t
  tlimit[p, t] = limit
  tprice[p, t] = $3
  tunits[p, t] = units($3)
  next
}

file == "accounts" {
  if (NF != 3 && NF != 4)
    bad()
  if (!is_name($1) || ($1 in aplan) || !($2 in pmodel) || !is_date($3))
    bad()
  if (NF == 4 && (!is_date($4) || $4 <= $3))
    bad()
  acct[++nacct] = $1
  aplan[$1] = $2
  astart[$1] = $3
  aend[$1] = NF == 4 ? $4 : ""
  next
}

file == "usage" {
  if (NF != 3 || !($1 in aplan) || !is_date($2) || !is_count($3))
    bad()
  nuse++
  uacct[nuse] = $1
  udate[nuse] = $2
  uqty[nuse] = $3 + 0
  next
}

file == "discounts" {
  if (NF != 3 || !($1 in aplan))
    bad()
  if ($2 == "percent" && $3 ~ /^([1-9][0-9]?|100)$/)
    value = $3 + 0
  else if ($2 == "fixed" && is_money($3))
    value = units($3)
  else
    bad()
  ndisc++
  dacct[ndisc] = $1
  dkind[ndisc] = $2
  dvalue[ndisc] = value
  next
}

file == "credits" {
  if (NF != 4 || !($1 in aplan) || !is_date($2) || !is_money($3) || !is_count($4))
    bad()
  ngrant++
  gacct[ngrant] = $1
  gdate[ngrant] = $2
  gleft[ngrant] = units($3) / 100
  gdays[ngrant] = $4 + 0
  next
}

# ---------------------------------------------------------------------
# Invoices

# The invoice being worked out: total is the sum of its line items so
# far, in units, and shown is 1 if it is printed (an invoice for PERIOD)
# or 0 if it is only worked out for its effect on later ones.

# out(s): print line s of the current invoice, if it is shown.
function out(s) {
  if (shown)
    print s
}

# item(label, u): add a line item of u units to the current invoice.
function item(label, u,    c) {
  c = cents(u)
  out(label " " money(c))
  total += c * 100
}

# remaining(): what the current invoice comes to so far, in cents.
function remaining() {
  return cents(total)
}

# quantity(a, from, to): account a's billable usage from date from to
# date to. Usage in the account's trial is free.
function quantity(a, from, to,    i, q, free) {
  free = add_months(astart[a], ptrial[aplan[a]])
  q = 0
  for (i = 1; i <= nuse; i++) {
    if (uacct[i] != a)
      continue
    if (udate[i] < from || udate[i] >= to)
      continue
    if (udate[i] < free)
      continue
    q += uqty[i]
  }
  return q
}

# invoice(a, ps, pe, show): work out account a's invoice for the period
# from ps to pe, and print it if show is 1.
function invoice(a, ps, pe, show,    p, stop, q) {
  shown = show
  total = 0
  p = aplan[a]
  stop = pe
  if (aend[a] != "" && aend[a] < pe)
    stop = aend[a]
  out("invoice " a " " ps " " pe)
  fee_item(a, ps, pe, stop)
  q = quantity(a, ps, stop)
  if (q > 0 && pmodel[p] == "graduated")
    graduated(p, q)
  if (q > 0 && pmodel[p] == "volume")
    volume(p, q)
  apply_discounts(a)
  apply_credits(a, pe)
  out("total " money(remaining()))
}

# bill_account(a): work out account a's invoices from its first period
# up to the one that starts in PERIOD, and print that one.
function bill_account(a,    k, last, ps, pe) {
  last = month_index(period) - month_index(astart[a])
  for (k = 0; k <= last; k++) {
    ps = add_months(astart[a], k)
    if (aend[a] != "" && ps >= aend[a])
      return
    pe = add_months(astart[a], k + 1)
    invoice(a, ps, pe, k == last)
  }
}

END {
  if (failed)
    exit failed
  for (i = 1; i <= nacct; i++)
    bill_account(acct[i])
}
BILL_EOF
