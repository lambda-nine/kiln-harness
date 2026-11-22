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
