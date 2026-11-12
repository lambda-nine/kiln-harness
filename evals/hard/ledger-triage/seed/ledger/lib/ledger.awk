# ledger.awk: the engine behind bin/ledger, a plain-text double-entry
# ledger. docs/MANUAL.md is the specification of everything here.
#
# bin/ledger runs it as
#
#   awk -v cmd=COMMAND -f lib/ledger.awk < FILE
#
# with PREFIX, if any, in the environment. The whole journal is read and
# checked first, line by line; the command itself runs in END.
#
# Amounts are kept as numbers of dollars.

# Data, filled in while reading:
#
#   tdate[t], tpayee[t]   transaction t's date and payee
#   tline[t]              the line its header is on
#   tfirst[t]             its first posting; its last is the one before
#                         the next transaction's first
#   pacct[p], pamt[p]     posting p's account and amount
#   ptx[p], pline[p]      its transaction and the line it is on
#   pelided[p]            1 if its amount was left out; finish() then
#                         sets pamt[p] to the balancing amount
#
# and, for the balance command:
#
#   total[a]              the sum of the selected postings to account a
#                         and to every account below it
#   kids[a]               the segments of a's direct children, each
#                         preceded by SUBSEP; kids[""] is the top level

BEGIN {
  failed = 0    # exit status once a problem has been reported
  ntx = 0       # transactions read so far
  np = 0        # postings read so far
  open = 0      # the transaction being read, or 0 between transactions
  prefix = ENVIRON["PREFIX"]
}

# die(code, message): report a problem on standard error and stop. END
# still runs after an exit in a main rule, so it checks `failed` first.
function die(code, message) {
  if (failed) return
  print message | "cat 1>&2"
  close("cat 1>&2")
  failed = code
  exit code
}

function syntax() {
  die(2, "syntax error at line " NR)
}

# ---------------------------------------------------------------------
# Amounts

# valid_amount(s): s is a written amount such as $12.00, $-0.50 or
# $1,234,567.89.
function valid_amount(s) {
  return s ~ /^\$-?([0-9]+|[0-9][0-9]?[0-9]?(,[0-9][0-9][0-9])+)\.[0-9][0-9]$/
}

# amount(s): the value of a valid written amount, in dollars.
function amount(s,    neg) {
  neg = substr(s, 2, 1) == "-"
  s = substr(s, neg ? 3 : 2)
  sub(/,/, "", s)
  return (neg ? -1 : 1) * s
}

# money(d): d dollars written the way the manual prints amounts.
function money(d,    s, neg, whole, frac, out) {
  s = sprintf("%.2f", d)
  neg = substr(s, 1, 1) == "-"
  if (neg) s = substr(s, 2)
  whole = substr(s, 1, index(s, ".") - 1)
  frac = substr(s, index(s, ".") + 1)
  out = ""
  while (length(whole) > 3) {
    out = "," substr(whole, length(whole) - 2) out
    whole = substr(whole, 1, length(whole) - 3)
  }
  return "$" (neg ? "-" : "") whole out "." frac
}

# ---------------------------------------------------------------------
# Accounts

# valid_account(a): a is one or more segments separated by `:`, none of
# them empty or starting or ending with a space.
function valid_account(a,    n, seg, i) {
  n = split(a, seg, ":")
  if (n == 0) return 0
  for (i = 1; i <= n; i++)
    if (seg[i] == "" || seg[i] ~ /^ / || seg[i] ~ / $/) return 0
  return 1
}

# matches(a): account a is selected by the command's PREFIX.
function matches(a) {
  if (prefix == "") return 1
  return index(a, prefix) == 1
}

# ---------------------------------------------------------------------
# Reading the journal

# days_in(year, month): the number of days in that month of the
# Gregorian calendar.
function days_in(year, month) {
  if (month == 2) {
    if (year % 400 == 0) return 29
    if (year % 100 == 0) return 28
    return year % 4 == 0 ? 29 : 28
  }
  if (month == 4 || month == 6 || month == 9 || month == 11) return 30
  return 31
}

# valid_date(d): d is YYYY-MM-DD and names a real day.
function valid_date(d,    year, month, day) {
  if (d !~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/) return 0
  year = substr(d, 1, 4) + 0
  month = substr(d, 6, 2) + 0
  day = substr(d, 9, 2) + 0
  if (month < 1 || month > 12) return 0
  return day >= 1 && day <= days_in(year, month)
}

# header(line): a line that starts a transaction, `YYYY-MM-DD PAYEE`.
function header(line,    date, payee) {
  finish()
  if (line !~ /^[^ \t]+[ \t]+[^ \t]/) syntax()
  date = line
  sub(/[ \t].*/, "", date)
  if (!valid_date(date)) syntax()
  payee = substr(line, length(date) + 1)
  sub(/^[ \t]+/, "", payee)
  ntx++
  tdate[ntx] = date
  tpayee[ntx] = payee
  tline[ntx] = NR
  tfirst[ntx] = np + 1
  open = ntx
}

# posting(line): an indented line `ACCOUNT  AMOUNT` or `ACCOUNT`, either
# of them perhaps followed by a comment.
function posting(line,    s, acct, amt) {
  if (!open) syntax()
  s = line
  if (match(s, /[ \t];/)) s = substr(s, 1, RSTART - 1)
  sub(/[ \t]+$/, "", s)
  sub(/^[ \t]+/, "", s)
  if (match(s, /\t|  /)) {
    acct = substr(s, 1, RSTART - 1)
    amt = substr(s, RSTART)
    sub(/^[ \t]+/, "", amt)
  } else {
    acct = s
    amt = ""
  }
  if (!valid_account(acct)) syntax()
  np++
  pacct[np] = acct
  ptx[np] = open
  pline[np] = NR
  if (amt == "") {
    pelided[np] = 1
    pamt[np] = 0
    return
  }
  if (!valid_amount(amt)) syntax()
  pelided[np] = 0
  pamt[np] = amount(amt)
}

# finish(): the open transaction is complete. Fill in its elided amount,
# if it has one, and make sure it balances.
function finish(    p, sum, count, blanks, hole) {
  if (!open) return
  sum = 0
  count = 0
  blanks = 0
  for (p = tfirst[open]; p <= np; p++) {
    count++
    if (pelided[p]) {
      blanks++
      hole = p
    } else {
      sum += pamt[p]
    }
  }
  if (count < 2 || blanks > 1) unbalanced()
  if (blanks == 1) pamt[hole] = -sum
  else if (sum > 0.005 || sum < -0.005) unbalanced()
  open = 0
}

function unbalanced() {
  die(1, "unbalanced transaction at line " pline[np])
}

{
  line = $0
  sub(/[ \t]+$/, "", line)
}
line == "" { finish(); next }
line ~ /^[ \t]*;/ { next }
line ~ /^[0-9]/ { header(line); next }
line ~ /^[ \t]/ { posting(line); next }
{ syntax() }

# ---------------------------------------------------------------------
# Commands

# sort_names(a, n): sort a[1..n] in place, byte by byte.
function sort_names(a, n,    i, j, v) {
  for (i = 2; i <= n; i++) {
    v = a[i]
    j = i - 1
    while (j >= 1 && (a[j] "") > (v "")) {
      a[j + 1] = a[j]
      j--
    }
    a[j + 1] = v
  }
}

function spaces(n,    s) {
  s = ""
  while (n-- > 0) s = s " "
  return s
}

# tree(node, depth): print the children of node (the top level when node
# is ""), each followed by its own children.
function tree(node, depth,    list, n, i, name) {
  if (!(node in kids)) return
  n = split(substr(kids[node], 2), list, SUBSEP)
  sort_names(list, n)
  for (i = 1; i <= n; i++) {
    name = node == "" ? list[i] : node ":" list[i]
    printf "%14s  %s%s\n", money(total[name]), spaces(2 * depth), list[i]
    tree(name, depth + 1)
  }
}

function balance(    p, n, seg, i, node, parent, grand) {
  grand = 0
  for (p = 1; p <= np; p++) {
    if (!matches(pacct[p])) continue
    grand += pamt[p]
    n = split(pacct[p], seg, ":")
    node = ""
    for (i = 1; i <= n; i++) {
      parent = node
      node = i == 1 ? seg[1] : node ":" seg[i]
      if (!(node in total)) {
        total[node] = 0
        kids[parent] = kids[parent] SUBSEP seg[i]
      }
      total[node] += pamt[p]
    }
  }
  tree("", 0)
  print "--------------"
  printf "%14s\n", money(grand)
}

function register(    k, i, j, v, best, list, p, run) {
  k = 0
  for (p = 1; p <= np; p++)
    if (matches(pacct[p])) list[++k] = p
  # Date order; postings with the same date keep their order in the file.
  for (i = 1; i < k; i++) {
    best = i
    for (j = i + 1; j <= k; j++)
      if (tdate[ptx[list[j]]] < tdate[ptx[list[best]]]) best = j
    if (best != i) {
      v = list[i]
      list[i] = list[best]
      list[best] = v
    }
  }
  run = 0
  for (i = 1; i <= k; i++) {
    p = list[i]
    run += pamt[p]
    printf "%-10s %-20s %-24s %13s %13s\n", tdate[ptx[p]], substr(tpayee[ptx[p]], 1, 20), pacct[p], money(pamt[p]), money(run)
  }
}

END {
  if (failed) exit failed
  finish()
  if (cmd == "check") printf "ok: %d transactions\n", ntx
  else if (cmd == "balance") balance()
  else if (cmd == "register") register()
  else die(2, "usage: ledger check FILE | balance FILE [PREFIX] | register FILE PREFIX")
}
