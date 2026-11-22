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
  pfee[$1] = units($2) / 100
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
    value = units($3) / 100
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

# item(label, c): add a line item of c cents to the current invoice.
function item(label, c) {
  out(label " " money(c))
  total += c
}

# remaining(): what the current invoice comes to so far, in cents.
function remaining() {
  return total
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
