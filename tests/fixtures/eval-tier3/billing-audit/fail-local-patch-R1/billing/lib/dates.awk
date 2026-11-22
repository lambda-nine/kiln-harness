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
  return day_number(b) - day_number(a) + 1
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
