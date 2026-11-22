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
