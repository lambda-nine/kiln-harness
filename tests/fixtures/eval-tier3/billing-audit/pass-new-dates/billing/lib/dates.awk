# dates.awk: calendar arithmetic for bill.
#
# Dates are strings YYYY-MM-DD. This version numbers days with the
# civil-from-days method (years starting in March), and adds months by
# month index.

function leap(y) {
  return (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

function month_days(y, m) {
  return substr("312831303130313130313031", 2 * m - 1, 2) + (m == 2 && leap(y))
}

function date_y(s) { return substr(s, 1, 4) + 0 }
function date_m(s) { return substr(s, 6, 2) + 0 }
function date_d(s) { return substr(s, 9, 2) + 0 }

function make_date(y, m, d) {
  return sprintf("%04d-%02d-%02d", y, m, d)
}

function is_date(s) {
  if (s !~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/)
    return 0
  if (date_y(s) < 1 || date_m(s) < 1 || date_m(s) > 12)
    return 0
  return date_d(s) >= 1 && date_d(s) <= month_days(date_y(s), date_m(s))
}

function month_index(s) {
  return (substr(s, 1, 4) + 0) * 12 + substr(s, 6, 2) - 1
}

# day_number(s): days since 0000-03-01 in the proleptic calendar.
function day_number(s,    y, m, d, era, yoe, doy) {
  y = date_y(s)
  m = date_m(s)
  d = date_d(s)
  if (m <= 2)
    y--
  era = int(y / 400)
  yoe = y - era * 400
  doy = int((153 * (m > 2 ? m - 3 : m + 9) + 2) / 5) + d - 1
  return era * 146097 + yoe * 365 + int(yoe / 4) - int(yoe / 100) + doy
}

function days_between(a, b) {
  return day_number(b) - day_number(a)
}

function add_months(s, k,    i, y, m, d) {
  i = month_index(s) + k
  y = int(i / 12)
  m = i - y * 12 + 1
  d = date_d(s)
  if (d > month_days(y, m))
    d = month_days(y, m)
  return make_date(y, m, d)
}
