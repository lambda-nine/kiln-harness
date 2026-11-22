# prorate.awk: the fee line of an invoice.
#
# Every invoice charges its plan's monthly fee. An account that ends
# inside a period pays only for the part of the period before its end
# date (MANUAL section 5): the fee times the days it was active over the
# days in the period. The result is usually not a whole number of units;
# item() rounds it like any other line item.

# fee_item(a, ps, pe, stop): charge account a's fee for the period from
# ps to pe, where stop is pe or, if the account ends inside the period,
# its end date.
function fee_item(a, ps, pe, stop,    p, used, len) {
  p = aplan[a]
  if (stop == pe) {
    item("fee " p, pfee[p])
    return
  }
  used = days_between(ps, stop) - 1
  len = days_between(ps, pe) - 1
  item("fee " p " " used "/" len, pfee[p] * used / len)
}
