# invoice.awk: invoices and reports from a CSV file of invoice lines.
#
# Usage:
#   awk -f invoice.awk [-v report=NAME] [-v today=YYYY-MM-DD] FILE...
#
# Input
# -----
# Each record is one invoice line with 8 or 9 comma-separated fields:
#
#   invoice,date,customer,region,category,description,qty,unit_price[,code]
#
#   invoice      the invoice id; lines with the same id form one invoice, in
#                the order they appear. Every line of an invoice must name
#                the same customer.
#   date         the line's date, YYYY-MM-DD; an invoice is dated by its
#                earliest line.
#   customer     the customer's name.
#   region       a two-letter US state code, or DC, for the sales tax.
#   category     what was sold: general, food, clothing, books, medicine,
#                software or services. Categories decide exemptions and
#                reduced rates (see Tax).
#   description  free text.
#   qty          a whole number other than 0; negative for a refund.
#   unit_price   a decimal amount with at most two decimals, not negative.
#   code         optional: a discount code (see Discounts).
#
# Fields follow the usual CSV rules: a field may be enclosed in double
# quotes, and then may hold commas; inside such a field, two double quotes
# in a row stand for one double quote character. Spaces around a field are
# trimmed. A CRLF line ending is accepted. Blank lines, lines that start
# with # and a first line that starts with "invoice," (a header row) are
# skipped.
#
# A line that cannot be used is reported on stderr as
#   invoice.awk: line N: REASON
# and left out; the reports are still printed, and the exit status is 1.
#
# Amounts
# -------
# All arithmetic is in whole cents. For each line:
#
#   amount   = qty * unit_price
#   discount = amount * discount rate, rounded to the cent
#   net      = amount - discount
#   tax      = net * tax rate, rounded to the cent
#
# Rounding is to the nearest cent, and an exact half cent rounds away from
# zero, so 0.5 cents becomes 1 cent and -0.5 cents becomes -1 cent. A
# refund line (negative qty) therefore has a negative amount, discount, net
# and tax that mirror a sale of the same size exactly. Totals are sums of
# the rounded line values.
#
# Tax
# ---
# Each region has a standard rate. A category exempt in a region pays no
# tax there. Food and medicine pay a region's reduced rate when it has one.
# Rates are listed in load_tables() below.
#
# Discounts
# ---------
# A code gives a fixed percentage off the line (see DISCOUNT below), except
# VOLUME, which gives 2% from 10 items, 5% from 50 items and 8% from 100
# items, counting the items of a refund the same way.
#
# Due dates
# ---------
# An invoice is due its customer's payment terms after its date: 30 days
# unless TERMS below lists the customer. A due date that falls on a
# Saturday, a Sunday or a listed holiday moves to the next business day.
#
# Reports
# -------
# -v report=NAME picks one; the default is invoice.
#
#   invoice     each invoice with its lines, subtotal, discounts, tax and
#               total; amounts as 1,234.56.
#   summary     invoices, net, tax and total per customer, by name, with
#               each customer's share of the grand total.
#   monthly     net, tax and total per month of the line dates.
#   tax         taxable and exempt net amounts and tax per region.
#   aging       invoices with a positive total by how far past due they are
#               on -v today=YYYY-MM-DD: current, 1-30, 31-60, 61-90 or 90+
#               days.
#   statement   per customer, the invoices by date with a running balance.
#   customers   per customer: terms, invoice and line counts, first and last
#               invoice dates, the average gap between invoices in days, and
#               the largest invoice.
#   markdown    one Markdown table of the invoices.
#   csv         every line with its computed amounts, as CSV.
#   json        every invoice with its lines, as a JSON array.
#
# Machine-readable reports (csv and json) write amounts as plain decimals,
# such as 1234.56 or -0.05, and text exactly as it was given, escaped as the
# format requires.
#
# Examples
# --------
# Given orders.csv:
#
#   invoice,date,customer,region,category,description,qty,unit_price,code
#   A-1,2026-03-02,Acme Corp,CA,general,Widget,3,19.99,
#   A-1,2026-03-02,Acme Corp,CA,food,Coffee beans,2,12.50,SAVE10
#   B-2,2026-03-05,"Birch & Daughters",TX,general,"Chairs, oak",12,45.00,VOLUME
#
# the line amounts are 59.97, 25.00 and 540.00. The coffee gets 2.50 off
# (10%) and is exempt in CA; the chairs get 10.80 off (VOLUME, 2% for 12
# items). The widget pays 7.25% of 59.97 = 4.347825, so 4.35 of tax; the
# chairs pay 6.25% of 529.20 = 33.075, so 33.08. So:
#
#   awk -f invoice.awk -v report=summary orders.csv
#
# prints
#
#   Customer                 Invoices            Net          Tax          Total  Share
#   -----------------------------------------------------------------------------------
#   Acme Corp                       1          82.47         4.35          86.82    13%
#   Birch & Daughters               1         529.20        33.08         562.28    87%
#   -----------------------------------------------------------------------------------
#   All customers                   2         611.67        37.43         649.10
#
# A refund of one widget the next day,
#
#   A-3,2026-03-03,Acme Corp,CA,general,Widget (returned),-1,19.99,
#
# has an amount of -19.99 and a tax of -1.45 (7.25% of -19.99 is
# -1.449275), the exact mirror of selling one.
#
# Exit status: 0 when every line was used, 1 when some line was rejected,
# 2 for a usage error (an unknown report, or aging without a valid today).
#
# ---------------------------------------------------------------------------
# Strings
# ---------------------------------------------------------------------------

# trim(s): s without leading and trailing spaces and tabs.
function trim(s) {
  sub(/^[ \t]+/, "", s)
  sub(/[ \t]+$/, "", s)
  return s
}

# repeat(s, n): s written n times; "" when n is 0 or less.
function repeat(s, n,   out) {
  out = ""
  while (n-- > 0) out = out s
  return out
}

# pad_right(s, w): s followed by spaces up to width w. Longer strings are
# returned whole.
function pad_right(s, w) {
  if (length(s) >= w) return s
  return s repeat(" ", w - length(s))
}

# pad_left(s, w): spaces then s, up to width w. Longer strings are returned
# whole.
function pad_left(s, w) {
  if (length(s) >= w) return s
  return repeat(" ", w - length(s)) s
}

# center(s, w): s with enough spaces before it to center it in width w; an
# odd leftover space counts as being on the right. Nothing is added after s.
function center(s, w,   left) {
  if (length(s) >= w) return s
  left = int((w - length(s)) / 2)
  return repeat(" ", left) s
}

# clip(s, w): s cut to w characters, the last one replaced by "~" when
# anything was cut.
function clip(s, w) {
  if (length(s) <= w) return s
  if (w <= 1) return substr(s, 1, w)
  return substr(s, 1, w - 1) "~"
}

# upper_first(s): s with its first character in upper case.
function upper_first(s) {
  return toupper(substr(s, 1, 1)) substr(s, 2)
}

# is_digits(s): true when s is one or more decimal digits.
function is_digits(s) {
  return s ~ /^[0-9]+$/
}

# is_int(s): true when s is an optional minus sign and digits.
function is_int(s) {
  return s ~ /^-?[0-9]+$/
}

# str_less(a, b): true when a sorts before b as text, whatever they look
# like.
function str_less(a, b) {
  return (a "") < (b "")
}

# sort_list(list, n): sort list[1..n] as text, in place. The lists here
# are short, so an insertion sort is enough.
function sort_list(list, n,   i, j, v) {
  for (i = 2; i <= n; i++) {
    v = list[i]
    j = i - 1
    while (j >= 1 && str_less(v, list[j])) {
      list[j + 1] = list[j]
      j--
    }
    list[j + 1] = v
  }
}

# wrap_words(s, w, out): split s into lines of at most w characters at
# spaces, into out[1..n]; returns n. A word longer than w gets a line of its
# own.
function wrap_words(s, w, out,   n, nw, words, i, line) {
  n = 0
  line = ""
  nw = split(s, words, " ")
  for (i = 1; i <= nw; i++) {
    if (line == "") line = words[i]
    else if (length(line) + 1 + length(words[i]) <= w) line = line " " words[i]
    else { out[++n] = line; line = words[i] }
  }
  if (line != "" || n == 0) out[++n] = line
  return n
}

# ---------------------------------------------------------------------------
# CSV
# ---------------------------------------------------------------------------

# csv_split(line, fields): split one CSV record into fields[1..n] and return
# n, or -1 when a quoted field is not closed. Fields may be quoted with
# double quotes; inside quotes, commas are data and a doubled quote stands
# for one quote character. Spaces are kept as they are.
function csv_split(line, fields,   n, i, c, len, field, quoted) {
  n = 0
  field = ""
  quoted = 0
  len = length(line)
  for (i = 1; i <= len; i++) {
    c = substr(line, i, 1)
    if (quoted) {
      if (c == "\"") {
        if (substr(line, i + 1, 1) == "\"") {
          i++
        } else {
          quoted = 0
        }
      } else {
        field = field c
      }
    } else if (c == "\"") {
      quoted = 1
    } else if (c == ",") {
      fields[++n] = field
      field = ""
    } else {
      field = field c
    }
  }
  if (quoted) return -1
  fields[++n] = field
  return n
}

# csv_quote(s): s as one CSV field, quoted when it holds a comma, a quote,
# or leading or trailing spaces.
function csv_quote(s,   out, i, c) {
  if (s !~ /[,"]/ && s !~ /^ / && s !~ / $/) return s
  out = "\""
  for (i = 1; i <= length(s); i++) {
    c = substr(s, i, 1)
    if (c == "\"") out = out "\"\""
    else out = out c
  }
  return out "\""
}

# ---------------------------------------------------------------------------
# Dates
#
# Dates are proleptic Gregorian, written YYYY-MM-DD. Internally a date is a
# day number: days since 1970-01-01, negative before it.
# ---------------------------------------------------------------------------

# is_leap(y): true for leap years.
function is_leap(y) {
  return (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

# days_in_month(m, y): 28 to 31.
function days_in_month(m, y) {
  if (m == 2) return is_leap(y) ? 29 : 28
  if (m == 4 || m == 6 || m == 9 || m == 11) return 30
  return 31
}

# parse_date(s, parts): split YYYY-MM-DD into parts["y"], parts["m"] and
# parts["d"] as numbers; returns 1 for a real calendar date, 0 otherwise.
function parse_date(s, parts,   y, m, d) {
  if (s !~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/) return 0
  y = substr(s, 1, 4) + 0
  m = substr(s, 6, 2) + 0
  d = substr(s, 9, 2) + 0
  if (m < 1 || m > 12) return 0
  if (d < 1 || d > days_in_month(m, y)) return 0
  parts["y"] = y
  parts["m"] = m
  parts["d"] = d
  return 1
}

# days_from_civil(y, m, d): the day number of a date.
function days_from_civil(y, m, d,   era, yoe, doy, doe) {
  if (m <= 2) y--
  era = int((y >= 0 ? y : y - 399) / 400)
  yoe = y - era * 400
  doy = int((153 * (m > 2 ? m - 3 : m + 9) + 2) / 5) + d - 1
  doe = yoe * 365 + int(yoe / 4) - int(yoe / 100) + doy
  return era * 146097 + doe - 719468
}

# civil_from_days(z, parts): the date of day number z, into parts["y"],
# parts["m"] and parts["d"].
function civil_from_days(z, parts,   era, doe, yoe, y, doy, mp, d, m) {
  z += 719468
  era = int((z >= 0 ? z : z - 146096) / 146097)
  doe = z - era * 146097
  yoe = int((doe - int(doe / 1460) + int(doe / 36524) - int(doe / 146096)) / 365)
  y = yoe + era * 400
  doy = doe - (365 * yoe + int(yoe / 4) - int(yoe / 100))
  mp = int((5 * doy + 2) / 153)
  d = doy - int((153 * mp + 2) / 5) + 1
  m = mp < 10 ? mp + 3 : mp - 9
  parts["y"] = m <= 2 ? y + 1 : y
  parts["m"] = m
  parts["d"] = d
}

# date_num(s): the day number of YYYY-MM-DD; the caller has validated it.
function date_num(s,   p) {
  parse_date(s, p)
  return days_from_civil(p["y"], p["m"], p["d"])
}

# date_str(z): day number z as YYYY-MM-DD.
function date_str(z,   p) {
  civil_from_days(z, p)
  return sprintf("%04d-%02d-%02d", p["y"], p["m"], p["d"])
}

# weekday(z): 1 (Monday) to 7 (Sunday) for day number z. 1970-01-01 was a
# Thursday.
function weekday(z,   w) {
  w = (z + 3) % 7
  if (w < 0) w += 7
  return w + 1
}

# weekday_name(n): Monday to Sunday for 1 to 7.
function weekday_name(n) {
  return WEEKDAYS[n]
}

# month_name(m): January to December for 1 to 12.
function month_name(m) {
  return MONTHS[m + 0]
}

# long_date(s): YYYY-MM-DD written out, as in "Friday, 3 April 2026".
function long_date(s,   p) {
  parse_date(s, p)
  return weekday_name(weekday(date_num(s))) ", " p["d"] " " month_name(p["m"]) " " p["y"]
}

# is_business_day(z): true on Monday to Friday unless day z is a listed
# holiday.
function is_business_day(z) {
  if (weekday(z) > 5) return 0
  return !((date_str(z)) in HOLIDAY)
}

# due_date(s, terms): the date terms days after s, moved forward to the next
# business day when it falls on a weekend or a holiday.
function due_date(s, terms,   z) {
  z = date_num(s) + terms
  while (!is_business_day(z)) z++
  return date_str(z)
}

# days_between(a, b): days from date a to date b, negative when b is
# earlier.
function days_between(a, b) {
  return date_num(b) - date_num(a)
}

# month_key(s): the YYYY-MM part of a date.
function month_key(s) {
  return substr(s, 1, 7)
}

# month_label(k): YYYY-MM written as "April 2026".
function month_label(k) {
  return month_name(substr(k, 6, 2) + 0) " " substr(k, 1, 4)
}

# ---------------------------------------------------------------------------
# Tables
# ---------------------------------------------------------------------------

# load_tables(): fill the lookup tables the rest of the program reads:
# RATE[region] and REDUCED[region] (basis points), REGION_NAME[region],
# EXEMPT[region, category], REDUCED_CATEGORY[category], DISCOUNT[code]
# (basis points), TERMS[customer] (days), HOLIDAY[date], WEEKDAYS[1..7] and
# MONTHS[1..12].
function load_tables(   n, i, list) {
  # Standard state sales tax rates, without local additions.
  RATE["AL"] = 400; REGION_NAME["AL"] = "Alabama"
  RATE["AK"] = 0; REGION_NAME["AK"] = "Alaska"
  RATE["AZ"] = 560; REGION_NAME["AZ"] = "Arizona"
  RATE["AR"] = 650; REGION_NAME["AR"] = "Arkansas"
  RATE["CA"] = 725; REGION_NAME["CA"] = "California"
  RATE["CO"] = 290; REGION_NAME["CO"] = "Colorado"
  RATE["CT"] = 635; REGION_NAME["CT"] = "Connecticut"
  RATE["DE"] = 0; REGION_NAME["DE"] = "Delaware"
  RATE["DC"] = 600; REGION_NAME["DC"] = "District of Columbia"
  RATE["FL"] = 600; REGION_NAME["FL"] = "Florida"
  RATE["GA"] = 400; REGION_NAME["GA"] = "Georgia"
  RATE["HI"] = 400; REGION_NAME["HI"] = "Hawaii"
  RATE["ID"] = 600; REGION_NAME["ID"] = "Idaho"
  RATE["IL"] = 625; REGION_NAME["IL"] = "Illinois"
  RATE["IN"] = 700; REGION_NAME["IN"] = "Indiana"
  RATE["IA"] = 600; REGION_NAME["IA"] = "Iowa"
  RATE["KS"] = 650; REGION_NAME["KS"] = "Kansas"
  RATE["KY"] = 600; REGION_NAME["KY"] = "Kentucky"
  RATE["LA"] = 500; REGION_NAME["LA"] = "Louisiana"
  RATE["ME"] = 550; REGION_NAME["ME"] = "Maine"
  RATE["MD"] = 600; REGION_NAME["MD"] = "Maryland"
  RATE["MA"] = 625; REGION_NAME["MA"] = "Massachusetts"
  RATE["MI"] = 600; REGION_NAME["MI"] = "Michigan"
  RATE["MN"] = 688; REGION_NAME["MN"] = "Minnesota"
  RATE["MS"] = 700; REGION_NAME["MS"] = "Mississippi"
  RATE["MO"] = 423; REGION_NAME["MO"] = "Missouri"
  RATE["MT"] = 0; REGION_NAME["MT"] = "Montana"
  RATE["NE"] = 550; REGION_NAME["NE"] = "Nebraska"
  RATE["NV"] = 685; REGION_NAME["NV"] = "Nevada"
  RATE["NH"] = 0; REGION_NAME["NH"] = "New Hampshire"
  RATE["NJ"] = 663; REGION_NAME["NJ"] = "New Jersey"
  RATE["NM"] = 488; REGION_NAME["NM"] = "New Mexico"
  RATE["NY"] = 400; REGION_NAME["NY"] = "New York"
  RATE["NC"] = 475; REGION_NAME["NC"] = "North Carolina"
  RATE["ND"] = 500; REGION_NAME["ND"] = "North Dakota"
  RATE["OH"] = 575; REGION_NAME["OH"] = "Ohio"
  RATE["OK"] = 450; REGION_NAME["OK"] = "Oklahoma"
  RATE["OR"] = 0; REGION_NAME["OR"] = "Oregon"
  RATE["PA"] = 600; REGION_NAME["PA"] = "Pennsylvania"
  RATE["RI"] = 700; REGION_NAME["RI"] = "Rhode Island"
  RATE["SC"] = 600; REGION_NAME["SC"] = "South Carolina"
  RATE["SD"] = 420; REGION_NAME["SD"] = "South Dakota"
  RATE["TN"] = 700; REGION_NAME["TN"] = "Tennessee"
  RATE["TX"] = 625; REGION_NAME["TX"] = "Texas"
  RATE["UT"] = 610; REGION_NAME["UT"] = "Utah"
  RATE["VT"] = 600; REGION_NAME["VT"] = "Vermont"
  RATE["VA"] = 530; REGION_NAME["VA"] = "Virginia"
  RATE["WA"] = 650; REGION_NAME["WA"] = "Washington"
  RATE["WV"] = 600; REGION_NAME["WV"] = "West Virginia"
  RATE["WI"] = 500; REGION_NAME["WI"] = "Wisconsin"
  RATE["WY"] = 400; REGION_NAME["WY"] = "Wyoming"

  # Reduced rates for food and medicine, where a state has one.
  REDUCED["AL"] = 200
  REDUCED["AR"] = 13
  REDUCED["IL"] = 100
  REDUCED["KS"] = 195
  REDUCED["MS"] = 500
  REDUCED["MO"] = 123
  REDUCED["NC"] = 200
  REDUCED["TN"] = 400
  REDUCED["UT"] = 300
  REDUCED["VA"] = 100
  REDUCED_CATEGORY["food"] = 1
  REDUCED_CATEGORY["medicine"] = 1

  # States where food is exempt.
  n = split("AZ CA CO CT DC FL GA ID IN IA KY LA", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "food"] = 1
  n = split("ME MD MA MI MN NE NV NJ NM NY ND OH", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "food"] = 1
  n = split("PA RI SC TX VT WA WV WI WY", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "food"] = 1

  # States where medicine is exempt.
  n = split("AK AZ CA CO CT DE DC FL GA HI ID IN", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "medicine"] = 1
  n = split("IA KY LA ME MD MA MI MN MT NE NV NH", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "medicine"] = 1
  n = split("NJ NM NY ND OH OK OR PA RI SC SD TX", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "medicine"] = 1
  n = split("VT WA WV WI WY", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "medicine"] = 1

  # States where clothing is exempt.
  n = split("MN NJ PA VT MA RI", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "clothing"] = 1

  # States where books is exempt.
  n = split("RI MA NJ", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "books"] = 1

  # States where software is exempt.
  n = split("CA FL GA IL MD MA MI MO NV NY OK OR", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "software"] = 1
  n = split("VA WY", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "software"] = 1

  # States where services is exempt.
  n = split("AL AK AZ AR CA CO DE FL GA ID IL IN", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "services"] = 1
  n = split("IA KS KY LA ME MD MA MI MN MS MO MT", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "services"] = 1
  n = split("NE NV NH NJ NY NC ND OH OK OR PA RI", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "services"] = 1
  n = split("SC TN UT VT VA WI WY", list, " ")
  for (i = 1; i <= n; i++) EXEMPT[list[i], "services"] = 1

  # Discount codes, in basis points off the line amount. VOLUME is handled
  # in discount_rate().
  DISCOUNT["SAVE5"] = 500
  DISCOUNT["SAVE10"] = 1000
  DISCOUNT["SAVE15"] = 1500
  DISCOUNT["SAVE20"] = 2000
  DISCOUNT["STAFF"] = 2500
  DISCOUNT["PARTNER"] = 1200
  DISCOUNT["WELCOME"] = 750
  DISCOUNT["LOYAL3"] = 300
  DISCOUNT["SCHOOL"] = 1800
  DISCOUNT["NONPROFIT"] = 2200
  DISCOUNT["SPRING"] = 650
  DISCOUNT["CLEARANCE"] = 4000
  DISCOUNT["BULK"] = 900
  DISCOUNT["REFERRAL"] = 450
  DISCOUNT["PRICEMATCH"] = 125

  # Payment terms in days, for customers that do not pay in 30.
  TERMS["Acme Corp"] = 45
  TERMS["Birch & Daughters"] = 15
  TERMS["Cobalt Labs"] = 60
  TERMS["Elm Street Books"] = 14
  TERMS["Fernwood Clinic"] = 45
  TERMS["Granite Works"] = 60
  TERMS["Harbor Cafe"] = 7
  TERMS["Indigo Studio"] = 21
  TERMS["Juniper Schools"] = 90
  TERMS["Kestrel Air"] = 45
  TERMS["Maple Grocers"] = 10
  TERMS["Northwind Traders"] = 60
  TERMS["Orchard Farms"] = 14
  TERMS["Quarry Supply"] = 45
  TERMS["Redwood Legal"] = 21
  TERMS["Tidewater Marine"] = 60
  TERMS["Umber Paints"] = 15
  TERMS["Vale Pharmacy"] = 7
  TERMS["Willow Daycare"] = 14
  TERMS["Xeno Robotics"] = 90
  TERMS["Yarrow Tea Co"] = 21

  # Holidays on which nothing falls due: US federal holidays as observed
  # on their calendar date.
  HOLIDAY["2020-01-01"] = "New Year's Day"
  HOLIDAY["2020-01-20"] = "Martin Luther King Jr. Day"
  HOLIDAY["2020-02-17"] = "Presidents' Day"
  HOLIDAY["2020-05-25"] = "Memorial Day"
  HOLIDAY["2020-06-19"] = "Juneteenth"
  HOLIDAY["2020-07-04"] = "Independence Day"
  HOLIDAY["2020-09-07"] = "Labor Day"
  HOLIDAY["2020-10-12"] = "Columbus Day"
  HOLIDAY["2020-11-11"] = "Veterans Day"
  HOLIDAY["2020-11-26"] = "Thanksgiving"
  HOLIDAY["2020-12-25"] = "Christmas Day"
  HOLIDAY["2021-01-01"] = "New Year's Day"
  HOLIDAY["2021-01-18"] = "Martin Luther King Jr. Day"
  HOLIDAY["2021-02-15"] = "Presidents' Day"
  HOLIDAY["2021-05-31"] = "Memorial Day"
  HOLIDAY["2021-06-19"] = "Juneteenth"
  HOLIDAY["2021-07-04"] = "Independence Day"
  HOLIDAY["2021-09-06"] = "Labor Day"
  HOLIDAY["2021-10-11"] = "Columbus Day"
  HOLIDAY["2021-11-11"] = "Veterans Day"
  HOLIDAY["2021-11-25"] = "Thanksgiving"
  HOLIDAY["2021-12-25"] = "Christmas Day"
  HOLIDAY["2022-01-01"] = "New Year's Day"
  HOLIDAY["2022-01-17"] = "Martin Luther King Jr. Day"
  HOLIDAY["2022-02-21"] = "Presidents' Day"
  HOLIDAY["2022-05-30"] = "Memorial Day"
  HOLIDAY["2022-06-19"] = "Juneteenth"
  HOLIDAY["2022-07-04"] = "Independence Day"
  HOLIDAY["2022-09-05"] = "Labor Day"
  HOLIDAY["2022-10-10"] = "Columbus Day"
  HOLIDAY["2022-11-11"] = "Veterans Day"
  HOLIDAY["2022-11-24"] = "Thanksgiving"
  HOLIDAY["2022-12-25"] = "Christmas Day"
  HOLIDAY["2023-01-01"] = "New Year's Day"
  HOLIDAY["2023-01-16"] = "Martin Luther King Jr. Day"
  HOLIDAY["2023-02-20"] = "Presidents' Day"
  HOLIDAY["2023-05-29"] = "Memorial Day"
  HOLIDAY["2023-06-19"] = "Juneteenth"
  HOLIDAY["2023-07-04"] = "Independence Day"
  HOLIDAY["2023-09-04"] = "Labor Day"
  HOLIDAY["2023-10-09"] = "Columbus Day"
  HOLIDAY["2023-11-11"] = "Veterans Day"
  HOLIDAY["2023-11-23"] = "Thanksgiving"
  HOLIDAY["2023-12-25"] = "Christmas Day"
  HOLIDAY["2024-01-01"] = "New Year's Day"
  HOLIDAY["2024-01-15"] = "Martin Luther King Jr. Day"
  HOLIDAY["2024-02-19"] = "Presidents' Day"
  HOLIDAY["2024-05-27"] = "Memorial Day"
  HOLIDAY["2024-06-19"] = "Juneteenth"
  HOLIDAY["2024-07-04"] = "Independence Day"
  HOLIDAY["2024-09-02"] = "Labor Day"
  HOLIDAY["2024-10-14"] = "Columbus Day"
  HOLIDAY["2024-11-11"] = "Veterans Day"
  HOLIDAY["2024-11-28"] = "Thanksgiving"
  HOLIDAY["2024-12-25"] = "Christmas Day"
  HOLIDAY["2025-01-01"] = "New Year's Day"
  HOLIDAY["2025-01-20"] = "Martin Luther King Jr. Day"
  HOLIDAY["2025-02-17"] = "Presidents' Day"
  HOLIDAY["2025-05-26"] = "Memorial Day"
  HOLIDAY["2025-06-19"] = "Juneteenth"
  HOLIDAY["2025-07-04"] = "Independence Day"
  HOLIDAY["2025-09-01"] = "Labor Day"
  HOLIDAY["2025-10-13"] = "Columbus Day"
  HOLIDAY["2025-11-11"] = "Veterans Day"
  HOLIDAY["2025-11-27"] = "Thanksgiving"
  HOLIDAY["2025-12-25"] = "Christmas Day"
  HOLIDAY["2026-01-01"] = "New Year's Day"
  HOLIDAY["2026-01-19"] = "Martin Luther King Jr. Day"
  HOLIDAY["2026-02-16"] = "Presidents' Day"
  HOLIDAY["2026-05-25"] = "Memorial Day"
  HOLIDAY["2026-06-19"] = "Juneteenth"
  HOLIDAY["2026-07-04"] = "Independence Day"
  HOLIDAY["2026-09-07"] = "Labor Day"
  HOLIDAY["2026-10-12"] = "Columbus Day"
  HOLIDAY["2026-11-11"] = "Veterans Day"
  HOLIDAY["2026-11-26"] = "Thanksgiving"
  HOLIDAY["2026-12-25"] = "Christmas Day"
  HOLIDAY["2027-01-01"] = "New Year's Day"
  HOLIDAY["2027-01-18"] = "Martin Luther King Jr. Day"
  HOLIDAY["2027-02-15"] = "Presidents' Day"
  HOLIDAY["2027-05-31"] = "Memorial Day"
  HOLIDAY["2027-06-19"] = "Juneteenth"
  HOLIDAY["2027-07-04"] = "Independence Day"
  HOLIDAY["2027-09-06"] = "Labor Day"
  HOLIDAY["2027-10-11"] = "Columbus Day"
  HOLIDAY["2027-11-11"] = "Veterans Day"
  HOLIDAY["2027-11-25"] = "Thanksgiving"
  HOLIDAY["2027-12-25"] = "Christmas Day"
  HOLIDAY["2028-01-01"] = "New Year's Day"
  HOLIDAY["2028-01-17"] = "Martin Luther King Jr. Day"
  HOLIDAY["2028-02-21"] = "Presidents' Day"
  HOLIDAY["2028-05-29"] = "Memorial Day"
  HOLIDAY["2028-06-19"] = "Juneteenth"
  HOLIDAY["2028-07-04"] = "Independence Day"
  HOLIDAY["2028-09-04"] = "Labor Day"
  HOLIDAY["2028-10-09"] = "Columbus Day"
  HOLIDAY["2028-11-11"] = "Veterans Day"
  HOLIDAY["2028-11-23"] = "Thanksgiving"
  HOLIDAY["2028-12-25"] = "Christmas Day"
  HOLIDAY["2029-01-01"] = "New Year's Day"
  HOLIDAY["2029-01-15"] = "Martin Luther King Jr. Day"
  HOLIDAY["2029-02-19"] = "Presidents' Day"
  HOLIDAY["2029-05-28"] = "Memorial Day"
  HOLIDAY["2029-06-19"] = "Juneteenth"
  HOLIDAY["2029-07-04"] = "Independence Day"
  HOLIDAY["2029-09-03"] = "Labor Day"
  HOLIDAY["2029-10-08"] = "Columbus Day"
  HOLIDAY["2029-11-11"] = "Veterans Day"
  HOLIDAY["2029-11-22"] = "Thanksgiving"
  HOLIDAY["2029-12-25"] = "Christmas Day"
  HOLIDAY["2030-01-01"] = "New Year's Day"
  HOLIDAY["2030-01-21"] = "Martin Luther King Jr. Day"
  HOLIDAY["2030-02-18"] = "Presidents' Day"
  HOLIDAY["2030-05-27"] = "Memorial Day"
  HOLIDAY["2030-06-19"] = "Juneteenth"
  HOLIDAY["2030-07-04"] = "Independence Day"
  HOLIDAY["2030-09-02"] = "Labor Day"
  HOLIDAY["2030-10-14"] = "Columbus Day"
  HOLIDAY["2030-11-11"] = "Veterans Day"
  HOLIDAY["2030-11-28"] = "Thanksgiving"
  HOLIDAY["2030-12-25"] = "Christmas Day"
  HOLIDAY["2031-01-01"] = "New Year's Day"
  HOLIDAY["2031-01-20"] = "Martin Luther King Jr. Day"
  HOLIDAY["2031-02-17"] = "Presidents' Day"
  HOLIDAY["2031-05-26"] = "Memorial Day"
  HOLIDAY["2031-06-19"] = "Juneteenth"
  HOLIDAY["2031-07-04"] = "Independence Day"
  HOLIDAY["2031-09-01"] = "Labor Day"
  HOLIDAY["2031-10-13"] = "Columbus Day"
  HOLIDAY["2031-11-11"] = "Veterans Day"
  HOLIDAY["2031-11-27"] = "Thanksgiving"
  HOLIDAY["2031-12-25"] = "Christmas Day"
  HOLIDAY["2032-01-01"] = "New Year's Day"
  HOLIDAY["2032-01-19"] = "Martin Luther King Jr. Day"
  HOLIDAY["2032-02-16"] = "Presidents' Day"
  HOLIDAY["2032-05-31"] = "Memorial Day"
  HOLIDAY["2032-06-19"] = "Juneteenth"
  HOLIDAY["2032-07-04"] = "Independence Day"
  HOLIDAY["2032-09-06"] = "Labor Day"
  HOLIDAY["2032-10-11"] = "Columbus Day"
  HOLIDAY["2032-11-11"] = "Veterans Day"
  HOLIDAY["2032-11-25"] = "Thanksgiving"
  HOLIDAY["2032-12-25"] = "Christmas Day"
  HOLIDAY["2033-01-01"] = "New Year's Day"
  HOLIDAY["2033-01-17"] = "Martin Luther King Jr. Day"
  HOLIDAY["2033-02-21"] = "Presidents' Day"
  HOLIDAY["2033-05-30"] = "Memorial Day"
  HOLIDAY["2033-06-19"] = "Juneteenth"
  HOLIDAY["2033-07-04"] = "Independence Day"
  HOLIDAY["2033-09-05"] = "Labor Day"
  HOLIDAY["2033-10-10"] = "Columbus Day"
  HOLIDAY["2033-11-11"] = "Veterans Day"
  HOLIDAY["2033-11-24"] = "Thanksgiving"
  HOLIDAY["2033-12-25"] = "Christmas Day"
  HOLIDAY["2034-01-01"] = "New Year's Day"
  HOLIDAY["2034-01-16"] = "Martin Luther King Jr. Day"
  HOLIDAY["2034-02-20"] = "Presidents' Day"
  HOLIDAY["2034-05-29"] = "Memorial Day"
  HOLIDAY["2034-06-19"] = "Juneteenth"
  HOLIDAY["2034-07-04"] = "Independence Day"
  HOLIDAY["2034-09-04"] = "Labor Day"
  HOLIDAY["2034-10-09"] = "Columbus Day"
  HOLIDAY["2034-11-11"] = "Veterans Day"
  HOLIDAY["2034-11-23"] = "Thanksgiving"
  HOLIDAY["2034-12-25"] = "Christmas Day"

  split("Monday Tuesday Wednesday Thursday Friday Saturday Sunday", WEEKDAYS, " ")
  split("January February March April May June July August September October " \
    "November December", MONTHS, " ")
}

# ---------------------------------------------------------------------------
# Money
#
# Amounts are whole cents, so sums are exact. Rates are basis points:
# 1 basis point is 0.01%, so 7.25% is 725.
# ---------------------------------------------------------------------------

# parse_money(s): the number of cents in a decimal amount such as 12, 12.5,
# 12.50 or -0.99, or "" when s is not one. At most two decimals.
function parse_money(s,   sign, whole, frac, dot) {
  sign = 1
  if (substr(s, 1, 1) == "-") { sign = -1; s = substr(s, 2) }
  dot = index(s, ".")
  if (dot) {
    whole = substr(s, 1, dot - 1)
    frac = substr(s, dot + 1)
  } else {
    whole = s
    frac = ""
  }
  if (whole == "") whole = "0"
  if (!is_digits(whole)) return ""
  if (dot && (frac == "" || length(frac) > 2 || !is_digits(frac))) return ""
  if (length(frac) == 1) frac = frac "0"
  if (frac == "") frac = "00"
  return sign * (whole * 100 + frac)
}

# round_div(n, d): n / d rounded to the nearest whole number, halves away
# from zero, for a positive d: 5/2 is 3 and -5/2 is -3.
function round_div(n, d,   q, r) {
  q = int(n / d)
  r = n - q * d
  if (r * 2 >= d) q++
  return q
}

# apply_rate(cents, bp): cents times a rate in basis points, rounded to whole
# cents with round_div.
function apply_rate(cents, bp) {
  return round_div(cents * bp, 10000)
}

# group3(digits): a string of digits with a comma between each group of
# three, from the right: 1234567 is 1,234,567.
function group3(digits,   out) {
  out = ""
  while (length(digits) > 3) {
    out = "," substr(digits, length(digits) - 2) out
    digits = substr(digits, 1, length(digits) - 3)
  }
  return digits out
}

# fmt_cents(c): cents written with a thousands separator and two decimals:
# 123456 is 1,234.56 and -5 is -0.05.
function fmt_cents(c,   sign, whole, frac) {
  sign = ""
  if (c < 0) { sign = "-"; c = -c }
  whole = int(c / 100)
  frac = c - whole * 100
  return sign group3(sprintf("%d", whole)) "." sprintf("%02d", frac)
}

# plain_cents(c): cents as a plain decimal for machine-readable output:
# 123456 is 1234.56 and -5 is -0.05.
function plain_cents(c,   sign, whole) {
  sign = ""
  if (c < 0) { sign = "-"; c = -c }
  whole = int(c / 100)
  return sign sprintf("%d.%02d", whole, c - whole * 100)
}

# fmt_rate(bp): a rate in basis points as a percentage, without trailing
# zeros: 725 is 7.25%, 600 is 6%, 0 is 0%.
function fmt_rate(bp,   s) {
  s = sprintf("%d.%02d", int(bp / 100), bp % 100)
  sub(/\.?0+$/, "", s)
  if (s == "") s = "0"
  return s "%"
}

# share(part, total): part as a whole percentage of total, rounded; 0 when
# total is 0.
function share(part, total) {
  if (total == 0) return 0
  return round_div(part * 100, total)
}

# ---------------------------------------------------------------------------
# Tax
# ---------------------------------------------------------------------------

# tax_rate(region, category): the rate in basis points for a category of goods
# in a region. A category exempt in the region pays nothing; a reduced
# category pays the region's reduced rate when it has one; everything else
# pays the region's standard rate. Unknown regions have no rate: -1.
function tax_rate(region, category) {
  if (!(region in RATE)) return -1
  if ((region SUBSEP category) in EXEMPT) return 0
  if (category in REDUCED_CATEGORY && region in REDUCED) return REDUCED[region]
  return RATE[region]
}

# line_tax(net, region, category): the tax on a line's net amount.
function line_tax(net, region, category) {
  return apply_rate(net, tax_rate(region, category))
}

# ---------------------------------------------------------------------------
# Discounts
# ---------------------------------------------------------------------------

# discount_rate(code, qty): the discount in basis points that a code gives a
# line of qty items. An empty code gives none. VOLUME depends on the
# quantity: 2% from 10 items, 5% from 50 and 8% from 100; refunds use the
# same tiers by the number of items returned. Unknown codes give -1.
function discount_rate(code, qty,   n) {
  if (code == "") return 0
  if (code == "VOLUME") {
    n = qty < 0 ? -qty : qty
    if (n >= 100) return 800
    if (n >= 50) return 500
    if (n >= 10) return 200
    return 0
  }
  if (code in DISCOUNT) return DISCOUNT[code]
  return -1
}

# line_discount(amount, code, qty): the discount on a line's amount, rounded
# to whole cents with round_div. It has the same sign as the amount.
function line_discount(amount, code, qty) {
  return apply_rate(amount, discount_rate(code, qty))
}

# ---------------------------------------------------------------------------
# Records
#
# Each accepted input line becomes line number L, with these fields:
#   L_INV[L], L_DATE[L], L_CUST[L], L_REGION[L], L_CAT[L], L_DESC[L],
#   L_QTY[L], L_PRICE[L] (cents), L_CODE[L], and the computed L_AMOUNT[L]
#   (qty times price), L_DISC[L], L_NET[L] (amount less discount),
#   L_RATE[L] and L_TAX[L].
# Invoices are numbered 1..NINV in order of first appearance; INV_ID[i] is
# the id, INV_OF[id] the number, INV_LINES[i] how many lines it has and
# INV_LINE[i, k] the k-th line number.
# ---------------------------------------------------------------------------

# reject(msg): report an input line that cannot be used and remember that
# one was rejected.
function reject(msg) {
  printf "invoice.awk: line %d: %s\n", NR, msg > "/dev/stderr"
  REJECTED++
}

# add_line(f): check one record's fields and store it; returns 1 when it was
# accepted.
function add_line(f,   L, inv, qty, price, disc, rate, i, d) {
  inv = trim(f[1])
  if (inv == "") { reject("missing invoice id"); return 0 }
  if (!parse_date(trim(f[2]), d)) { reject("bad date: " f[2]); return 0 }
  if (trim(f[3]) == "") { reject("missing customer"); return 0 }
  rate = tax_rate(trim(f[4]), trim(f[5]))
  if (rate < 0) { reject("unknown region: " f[4]); return 0 }
  qty = trim(f[7])
  if (!is_int(qty) || qty == 0) { reject("bad quantity: " f[7]); return 0 }
  price = parse_money(trim(f[8]))
  if (price == "") { reject("bad price: " f[8]); return 0 }
  if (price < 0) { reject("negative price: " f[8]); return 0 }
  disc = discount_rate(trim(f[9]), qty + 0)
  if (disc < 0) { reject("unknown discount code: " f[9]); return 0 }
  if (inv in INV_OF && INV_CUST[INV_OF[inv]] != trim(f[3])) {
    reject("invoice " inv " has another customer"); return 0
  }

  L = ++NLINES
  L_INV[L] = inv
  L_DATE[L] = trim(f[2])
  L_CUST[L] = trim(f[3])
  L_REGION[L] = trim(f[4])
  L_CAT[L] = trim(f[5])
  L_DESC[L] = trim(f[6])
  L_QTY[L] = qty + 0
  L_PRICE[L] = price
  L_CODE[L] = trim(f[9])
  L_AMOUNT[L] = L_QTY[L] * price
  L_DISC[L] = line_discount(L_AMOUNT[L], L_CODE[L], L_QTY[L])
  L_NET[L] = L_AMOUNT[L] - L_DISC[L]
  L_RATE[L] = rate
  L_TAX[L] = line_tax(L_NET[L], L_REGION[L], L_CAT[L])

  if (!(inv in INV_OF)) {
    i = ++NINV
    INV_OF[inv] = i
    INV_ID[i] = inv
    INV_CUST[i] = L_CUST[L]
    INV_DATE[i] = L_DATE[L]
  }
  i = INV_OF[inv]
  INV_LINE[i, ++INV_LINES[i]] = L
  # An invoice is dated by its earliest line.
  if (str_less(L_DATE[L], INV_DATE[i])) INV_DATE[i] = L_DATE[L]
  return 1
}

# invoice_totals(i, t): sums over invoice i into t["amount"], t["disc"],
# t["net"], t["tax"] and t["total"].
function invoice_totals(i, t,   k, L) {
  t["amount"] = t["disc"] = t["net"] = t["tax"] = 0
  for (k = 1; k <= INV_LINES[i]; k++) {
    L = INV_LINE[i, k]
    t["amount"] += L_AMOUNT[L]
    t["disc"] += L_DISC[L]
    t["net"] += L_NET[L]
    t["tax"] += L_TAX[L]
  }
  t["total"] = t["net"] + t["tax"]
}

# customer_terms(name): payment terms in days for a customer: TERMS when one
# is listed, 30 otherwise.
function customer_terms(name) {
  return (name in TERMS) ? TERMS[name] : 30
}

# ---------------------------------------------------------------------------
# Report: invoice
# ---------------------------------------------------------------------------

# rule(w): a line of w dashes.
function rule(w) {
  return repeat("-", w)
}

# invoice_line_row(L): one line item of the invoice report: description,
# quantity, unit price, amount and tax rate, with the description wrapped
# at 30 characters, and a second row for the discount when there is one.
function invoice_line_row(L,   desc, n, i, wrapped) {
  n = wrap_words(L_DESC[L], 30, wrapped)
  printf "  %-30s %5d %10s %12s %7s\n", wrapped[1], L_QTY[L], fmt_cents(L_PRICE[L]), \
    fmt_cents(L_AMOUNT[L]), fmt_rate(L_RATE[L])
  for (i = 2; i <= n; i++) printf "  %s\n", wrapped[i]
  if (L_DISC[L] != 0)
    printf "  %-30s %5s %10s %12s\n", "  less " L_CODE[L] " " fmt_rate(discount_rate(L_CODE[L], L_QTY[L])), \
      "", "", "(" fmt_cents(L_DISC[L]) ")"
}

# report_invoice(): every invoice in order of first appearance.
function report_invoice(   i, k, t, terms) {
  for (i = 1; i <= NINV; i++) {
    if (i > 1) print ""
    invoice_totals(i, t)
    terms = customer_terms(INV_CUST[i])
    print center("INVOICE " INV_ID[i], 72)
    print rule(72)
    printf "Customer: %s\n", INV_CUST[i]
    printf "Date:     %s (%s)\n", INV_DATE[i], long_date(INV_DATE[i])
    printf "Due:      %s (net %d)\n", due_date(INV_DATE[i], terms), terms
    print rule(72)
    printf "  %-30s %5s %10s %12s %7s\n", "Item", "Qty", "Price", "Amount", "Tax"
    for (k = 1; k <= INV_LINES[i]; k++) invoice_line_row(INV_LINE[i, k])
    print rule(72)
    printf "  %-47s %12s\n", "Subtotal", fmt_cents(t["amount"])
    if (t["disc"] != 0) printf "  %-47s %12s\n", "Discounts", "(" fmt_cents(t["disc"]) ")"
    printf "  %-47s %12s\n", "Tax", fmt_cents(t["tax"])
    printf "  %-47s %12s\n", "Total", fmt_cents(t["total"])
  }
}

# ---------------------------------------------------------------------------
# Report: summary
# ---------------------------------------------------------------------------

# report_summary(): one row per customer, by name, with the number of
# invoices, net, tax and total, then the grand totals and each customer's
# share of the grand total.
function report_summary(   i, n, names, name, t, cnt, net, tax, gnet, gtax, k) {
  n = 0
  for (i = 1; i <= NINV; i++) {
    name = INV_CUST[i]
    if (!(name in cnt)) names[++n] = name
    invoice_totals(i, t)
    cnt[name]++
    net[name] += t["net"]
    tax[name] += t["tax"]
    gnet += t["net"]
    gtax += t["tax"]
  }
  sort_list(names, n)
  printf "%-24s %8s %14s %12s %14s %6s\n", "Customer", "Invoices", "Net", "Tax", "Total", "Share"
  print rule(83)
  for (k = 1; k <= n; k++) {
    name = names[k]
    printf "%-24s %8d %14s %12s %14s %5d%%\n", clip(name, 24), cnt[name], fmt_cents(net[name]), \
      fmt_cents(tax[name]), fmt_cents(net[name] + tax[name]), share(net[name] + tax[name], gnet + gtax)
  }
  print rule(83)
  printf "%-24s %8d %14s %12s %14s\n", "All customers", NINV, fmt_cents(gnet), fmt_cents(gtax), \
    fmt_cents(gnet + gtax)
}

# ---------------------------------------------------------------------------
# Report: monthly
# ---------------------------------------------------------------------------

# report_monthly(): net, tax and total per calendar month of the line dates,
# oldest first, with a bar of # marks scaled to the largest month.
function report_monthly(   L, n, keys, key, net, tax, k, max, width) {
  n = 0
  for (L = 1; L <= NLINES; L++) {
    key = month_key(L_DATE[L])
    if (!(key in net)) { keys[++n] = key; net[key] = 0; tax[key] = 0 }
    net[key] += L_NET[L]
    tax[key] += L_TAX[L]
  }
  sort_list(keys, n)
  max = 0
  for (k = 1; k <= n; k++) if (net[keys[k]] + tax[keys[k]] > max) max = net[keys[k]] + tax[keys[k]]
  printf "%-16s %14s %12s %14s\n", "Month", "Net", "Tax", "Total"
  print rule(58)
  for (k = 1; k <= n; k++) {
    key = keys[k]
    width = max > 0 ? round_div((net[key] + tax[key] > 0 ? net[key] + tax[key] : 0) * 20, max) : 0
    printf "%-16s %14s %12s %14s%s\n", month_label(key), fmt_cents(net[key]), fmt_cents(tax[key]), \
      fmt_cents(net[key] + tax[key]), (width > 0 ? " " repeat("#", width) : "")
  }
}

# ---------------------------------------------------------------------------
# Report: tax
# ---------------------------------------------------------------------------

# report_tax(): per region, by code, the region's name, the taxable net
# amount (lines with a rate above 0), the exempt net amount and the tax.
function report_tax(   L, n, regions, r, taxable, exempt, tax, k, gt, ge, gx) {
  n = 0
  for (L = 1; L <= NLINES; L++) {
    r = L_REGION[L]
    if (!(r in tax)) { regions[++n] = r; tax[r] = 0; taxable[r] = 0; exempt[r] = 0 }
    if (L_RATE[L] > 0) taxable[r] += L_NET[L]
    else exempt[r] += L_NET[L]
    tax[r] += L_TAX[L]
  }
  sort_list(regions, n)
  printf "%-4s %-22s %7s %14s %14s %12s\n", "Code", "Region", "Rate", "Taxable", "Exempt", "Tax"
  print rule(78)
  for (k = 1; k <= n; k++) {
    r = regions[k]
    printf "%-4s %-22s %7s %14s %14s %12s\n", r, clip(REGION_NAME[r], 22), fmt_rate(RATE[r]), \
      fmt_cents(taxable[r]), fmt_cents(exempt[r]), fmt_cents(tax[r])
    gt += taxable[r]
    ge += exempt[r]
    gx += tax[r]
  }
  print rule(78)
  printf "%-4s %-22s %7s %14s %14s %12s\n", "", "All regions", "", fmt_cents(gt), fmt_cents(ge), fmt_cents(gx)
}

# ---------------------------------------------------------------------------
# Report: aging
# ---------------------------------------------------------------------------

# aging_bucket(days): the bucket for an invoice that many days past its due
# date: "current" when it is not yet due, then 1-30, 31-60, 61-90 and 90+.
function aging_bucket(days) {
  if (days <= 0) return "current"
  if (days <= 30) return "1-30"
  if (days <= 60) return "31-60"
  if (days <= 90) return "61-90"
  return "90+"
}

# report_aging(): each invoice with a positive total, by due date, with how
# many days it is past due on the date TODAY, and the total per bucket.
function report_aging(   i, n, list, t, due, late, b, sum, k, buckets, nb, j) {
  n = 0
  for (i = 1; i <= NINV; i++) {
    invoice_totals(i, t)
    if (t["total"] <= 0) continue
    due = due_date(INV_DATE[i], customer_terms(INV_CUST[i]))
    list[++n] = due " " sprintf("%06d", i)
  }
  sort_list(list, n)
  printf "%-12s %-24s %-10s %5s %-8s %14s\n", "Invoice", "Customer", "Due", "Late", "Bucket", "Total"
  print rule(78)
  for (k = 1; k <= n; k++) {
    split(list[k], j, " ")
    i = j[2] + 0
    due = j[1]
    invoice_totals(i, t)
    late = days_between(due, TODAY)
    b = aging_bucket(late)
    sum[b] += t["total"]
    printf "%-12s %-24s %-10s %5d %-8s %14s\n", clip(INV_ID[i], 12), clip(INV_CUST[i], 24), due, \
      (late > 0 ? late : 0), b, fmt_cents(t["total"])
  }
  print rule(78)
  nb = split("current 1-30 31-60 61-90 90+", buckets, " ")
  for (k = 1; k <= nb; k++) printf "%-8s %14s\n", buckets[k], fmt_cents(sum[buckets[k]] + 0)
}

# ---------------------------------------------------------------------------
# Report: csv
# ---------------------------------------------------------------------------

# report_csv(): every accepted line with its computed amounts, as CSV with a
# header row, in input order.
function report_csv(   L) {
  print "invoice,date,customer,region,category,description,qty,unit_price,amount,discount,net,tax_rate,tax"
  for (L = 1; L <= NLINES; L++) {
    print csv_quote(L_INV[L]) "," L_DATE[L] "," csv_quote(L_CUST[L]) "," L_REGION[L] "," \
      csv_quote(L_CAT[L]) "," csv_quote(L_DESC[L]) "," L_QTY[L] "," plain_cents(L_PRICE[L]) "," \
      plain_cents(L_AMOUNT[L]) "," plain_cents(L_DISC[L]) "," plain_cents(L_NET[L]) "," \
      fmt_rate(L_RATE[L]) "," plain_cents(L_TAX[L])
  }
}

# ---------------------------------------------------------------------------
# Report: statement
# ---------------------------------------------------------------------------

# report_statement(): for each customer, by name, every invoice by date
# (then id) with its total and a running balance, and the balance due.
function report_statement(   i, n, names, name, k, m, list, t, bal, j, seen, parts, cnt) {
  n = 0
  for (i = 1; i <= NINV; i++) {
    name = INV_CUST[i]
    if (!(name in seen)) { seen[name] = 1; names[++n] = name }
  }
  sort_list(names, n)
  for (k = 1; k <= n; k++) {
    name = names[k]
    m = 0
    split("", list)
    for (i = 1; i <= NINV; i++)
      if (INV_CUST[i] == name) list[++m] = INV_DATE[i] " " INV_ID[i] " " sprintf("%06d", i)
    sort_list(list, m)
    if (k > 1) print ""
    printf "Statement for %s (net %d)\n", name, customer_terms(name)
    printf "  %-10s %-14s %14s %14s\n", "Date", "Invoice", "Amount", "Balance"
    bal = 0
    cnt = 0
    for (j = 1; j <= m; j++) {
      split(list[j], parts, " ")
      i = parts[3] + 0
      invoice_totals(i, t)
      bal += t["total"]
      cnt++
      printf "  %-10s %-14s %14s %14s\n", INV_DATE[i], clip(INV_ID[i], 14), fmt_cents(t["total"]), \
        fmt_cents(bal)
    }
    printf "  %-25s %14s %14s\n", cnt " invoice" (cnt == 1 ? "" : "s"), "", fmt_cents(bal)
  }
}

# ---------------------------------------------------------------------------
# Report: markdown
# ---------------------------------------------------------------------------

# md_cell(s): s made safe for a Markdown table cell: a vertical bar is
# written \| and a backslash \\.
function md_cell(s,   out, i, c) {
  out = ""
  for (i = 1; i <= length(s); i++) {
    c = substr(s, i, 1)
    if (c == "|") out = out "\\|"
    else if (c == "\\") out = out "\\\\"
    else out = out c
  }
  return out
}

# report_markdown(): one Markdown table of every invoice, in order of first
# appearance, then a line with the grand total.
function report_markdown(   i, t, g) {
  print "| Invoice | Customer | Date | Net | Tax | Total |"
  print "|---|---|---|--:|--:|--:|"
  g = 0
  for (i = 1; i <= NINV; i++) {
    invoice_totals(i, t)
    g += t["total"]
    printf "| %s | %s | %s | %s | %s | %s |\n", md_cell(INV_ID[i]), md_cell(INV_CUST[i]), INV_DATE[i], \
      fmt_cents(t["net"]), fmt_cents(t["tax"]), fmt_cents(t["total"])
  }
  print ""
  printf "**Total: %s** over %d invoice%s.\n", fmt_cents(g), NINV, (NINV == 1 ? "" : "s")
}

# ---------------------------------------------------------------------------
# Report: customers
# ---------------------------------------------------------------------------

# report_customers(): one row per customer, by name: payment terms, the
# number of invoices and lines, the first and last invoice dates, the
# average number of days between consecutive invoices (or - with only one),
# and the largest invoice total.
function report_customers(   i, n, names, name, k, first, last, cnt, lines, big, t, gap, m, dates, j) {
  n = 0
  for (i = 1; i <= NINV; i++) {
    name = INV_CUST[i]
    invoice_totals(i, t)
    if (!(name in cnt)) {
      names[++n] = name
      cnt[name] = 0
      lines[name] = 0
      first[name] = last[name] = INV_DATE[i]
      big[name] = t["total"]
    }
    cnt[name]++
    lines[name] += INV_LINES[i]
    if (str_less(INV_DATE[i], first[name])) first[name] = INV_DATE[i]
    if (str_less(last[name], INV_DATE[i])) last[name] = INV_DATE[i]
    if (t["total"] > big[name]) big[name] = t["total"]
  }
  sort_list(names, n)
  printf "%-24s %5s %8s %5s %-10s %-10s %7s %14s\n", "Customer", "Terms", "Invoices", "Lines", \
    "First", "Last", "Avg gap", "Largest"
  print rule(90)
  for (k = 1; k <= n; k++) {
    name = names[k]
    if (cnt[name] > 1) {
      # The dates of this customer's invoices, in order, give the gaps.
      m = 0
      split("", dates)
      for (i = 1; i <= NINV; i++) if (INV_CUST[i] == name) dates[++m] = INV_DATE[i]
      sort_list(dates, m)
      gap = 0
      for (j = 2; j <= m; j++) gap += days_between(dates[j - 1], dates[j])
      gap = round_div(gap, m - 1)
    } else {
      gap = "-"
    }
    printf "%-24s %5d %8d %5d %-10s %-10s %7s %14s\n", clip(name, 24), customer_terms(name), cnt[name], \
      lines[name], first[name], last[name], gap, fmt_cents(big[name])
  }
}

# ---------------------------------------------------------------------------
# Report: json
# ---------------------------------------------------------------------------

# json_str(s): s as a JSON string, with its quotes. Quotes, backslashes and
# control characters are escaped; everything else is copied as it is.
function json_str(s,   out, i, c, len) {
  out = "\""
  len = length(s)
  for (i = 1; i <= len; i++) {
    c = substr(s, i, 1)
    if (c == "\"") out = out "\\\""
    else if (c == "\t") out = out "\\t"
    else if (c == "\r") out = out "\\r"
    else if (c == "\n") out = out "\\n"
    else if (c in CTRL) out = out sprintf("\\u%04x", CTRL[c])
    else out = out c
  }
  return out "\""
}

# json_line(L, last): one line item as a JSON object, followed by a comma
# unless it is the last.
function json_line(L, last) {
  printf "      {\"description\": %s, \"category\": %s, \"qty\": %d, \"unit_price\": %s, ", \
    json_str(L_DESC[L]), json_str(L_CAT[L]), L_QTY[L], plain_cents(L_PRICE[L])
  printf "\"discount\": %s, \"net\": %s, \"tax\": %s}%s\n", plain_cents(L_DISC[L]), \
    plain_cents(L_NET[L]), plain_cents(L_TAX[L]), (last ? "" : ",")
}

# report_json(): every invoice as a JSON object in an array, in order of
# first appearance, two-space indented.
function report_json(   i, k, t) {
  print "["
  for (i = 1; i <= NINV; i++) {
    invoice_totals(i, t)
    print "  {"
    printf "    \"invoice\": %s,\n", json_str(INV_ID[i])
    printf "    \"customer\": %s,\n", json_str(INV_CUST[i])
    printf "    \"date\": %s,\n", json_str(INV_DATE[i])
    printf "    \"due\": %s,\n", json_str(due_date(INV_DATE[i], customer_terms(INV_CUST[i])))
    print "    \"lines\": ["
    for (k = 1; k <= INV_LINES[i]; k++) json_line(INV_LINE[i, k], k == INV_LINES[i])
    print "    ],"
    printf "    \"net\": %s,\n", plain_cents(t["net"])
    printf "    \"tax\": %s,\n", plain_cents(t["tax"])
    printf "    \"total\": %s\n", plain_cents(t["total"])
    printf "  }%s\n", (i == NINV ? "" : ",")
  }
  print "]"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

BEGIN {
  load_tables()
  if (report == "") report = "invoice"
  if (report !~ /^(invoice|summary|monthly|tax|aging|statement|customers|markdown|csv|json)$/) {
    printf "invoice.awk: unknown report: %s\n", report > "/dev/stderr"
    BAD_USAGE = 1
    exit 2
  }
  if (report == "aging") {
    if (today == "" || !parse_date(today, TODAY_PARTS)) {
      print "invoice.awk: the aging report needs -v today=YYYY-MM-DD" > "/dev/stderr"
      BAD_USAGE = 1
      exit 2
    }
    TODAY = today
  }
  # Tab is handled by json_str itself; the other control characters 1-31
  # are written as \u escapes.
  for (i = 1; i < 32; i++) if (i != 9 && i != 10 && i != 13) CTRL[sprintf("%c", i)] = i
}

# Blank lines and lines starting with # are skipped, and so is a header row.
/^[ \t]*$/ || /^#/ { next }
FNR == 1 && /^invoice,/ { next }

{
  sub(/\r$/, "")
  n = csv_split($0, f)
  if (n < 0) { reject("unterminated quoted field"); next }
  if (n == 8) f[9] = ""
  if (n != 8 && n != 9) { reject("expected 8 or 9 fields, got " n); next }
  add_line(f)
  split("", f)
}

END {
  if (BAD_USAGE) exit 2
  if (report == "invoice") report_invoice()
  else if (report == "summary") report_summary()
  else if (report == "monthly") report_monthly()
  else if (report == "tax") report_tax()
  else if (report == "aging") report_aging()
  else if (report == "statement") report_statement()
  else if (report == "customers") report_customers()
  else if (report == "markdown") report_markdown()
  else if (report == "csv") report_csv()
  else if (report == "json") report_json()
  if (REJECTED) exit 1
}
