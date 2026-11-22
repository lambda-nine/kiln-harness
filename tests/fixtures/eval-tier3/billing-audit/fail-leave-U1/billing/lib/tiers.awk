# tiers.awk: usage charges for graduated and volume plans.
#
# A plan's tiers come from the tiers file, in order: tier t of plan p
# has the limit tlimit[p, t] (-1 for "*", no limit) and the unit price
# tprice[p, t] (as written) or tunits[p, t] (in units). ntier[p] is how
# many tiers the plan has. MANUAL section 6 describes both models.

# within(p, t, q): 1 if quantity q is inside the limit of plan p's tier t.
function within(p, t, q) {
  return tlimit[p, t] < 0 || q < tlimit[p, t]
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
