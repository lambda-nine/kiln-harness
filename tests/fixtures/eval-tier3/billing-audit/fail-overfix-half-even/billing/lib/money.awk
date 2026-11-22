# money.awk: amounts of money.
#
# bill works in "units" of one ten-thousandth of a dollar, which hold
# every price and amount in the data files exactly: 30.00 is 300000
# units and a unit price of 0.0125 is 125. A quantity times a price is
# then an exact whole number of units. Whole cents are 100 units each;
# cents() rounds a line item to them and money() prints cents.

# is_money(s): 1 if s is written as MONEY: digits, a point, two digits.
function is_money(s) {
  return s ~ /^[0-9]+\.[0-9][0-9]$/
}

# is_price(s): 1 if s is written as PRICE: digits, optionally a point
# and one to four more digits.
function is_price(s) {
  return s ~ /^[0-9]+(\.[0-9][0-9]?[0-9]?[0-9]?)?$/
}

# units(s): the value of a MONEY or PRICE string, in units.
function units(s,    i, frac) {
  i = index(s, ".")
  if (i == 0)
    return s * 10000
  frac = substr(s, i + 1)
  while (length(frac) < 4)
    frac = frac "0"
  return substr(s, 1, i - 1) * 10000 + frac
}

# cents(u): u units rounded to whole cents, half away from zero.
function cents(u,    neg, c, rest) {
  neg = u < 0
  if (neg)
    u = -u
  c = int(u / 100)
  rest = u - c * 100
  if (rest > 50 || (rest == 50 && c % 2 == 1))
    c++
  return neg ? -c : c
}

# money(c): c whole cents as bill prints money: 1234.50, -0.05, 0.00.
function money(c,    neg) {
  neg = c < 0
  if (neg)
    c = -c
  return (neg ? "-" : "") int(c / 100) "." sprintf("%02d", c % 100)
}
