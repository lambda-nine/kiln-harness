cd /workspace || exit 2
LC_ALL=C
export LC_ALL
# Regular files and directories only: no symlinks or other special entries.
[ -z "$(find . ! -type f ! -type d)" ] || { echo "not a regular file"; exit 1; }
# Only invoice.awk: no backup, test or temporary files.
[ "$(find . -type f)" = ./invoice.awk ] || { echo "files"; exit 1; }
# The file still ends with a newline and holds no NUL bytes, so the
# line-based comparisons below see every byte of it.
[ "$(tail -c 1 invoice.awk | od -An -tx1 | tr -d ' \n')" = 0a ] || { echo "final newline"; exit 1; }
[ "$(tr -d '\000' <invoice.awk | wc -c)" = "$(wc -c <invoice.awk)" ] || { echo "NUL bytes"; exit 1; }
tmp=$(mktemp -d) || exit 2
# split.awk cuts out each of the three functions with a bug: from its
# "function NAME(" line (spacing free) to the first line after it that is
# exactly "}" (the prompt asks for the closing brace at the start of its own
# line). With mode=outside each one becomes a marker line; with mode=ref, the
# fixed function from the directory d.
cat >"$tmp/split.awk" <<'END_SPLIT'
{ line[NR] = $0 }
END {
  n = NR
  i = 1
  while (i <= n) {
    if (line[i] !~ /^function[ \t]+(csv_split|round_div|json_str)[ \t]*\(/) {
      print line[i]
      i++
      continue
    }
    name = line[i]
    sub(/^function[ \t]+/, "", name)
    sub(/[ \t]*\(.*/, "", name)
    j = i + 1
    while (j <= n && line[j] != "}") j++
    if (mode == "ref") {
      f = d "/" name
      while ((getline l < f) > 0) print l
      close(f)
    } else {
      print "@@ " name
    }
    i = j + 1
  }
}
END_SPLIT
# Each of the three functions is there once, and every line outside them is
# as it was.
for fn in csv_split round_div json_str; do
  [ "$(awk -v fn="$fn" '$0 ~ ("^function[ \t]+" fn "[ \t]*[(]") { n++ } END { print n + 0 }' invoice.awk)" = 1 ] ||
    { rm -rf "$tmp"; echo "$fn"; exit 1; }
done
outside=$(awk -v mode=outside -f "$tmp/split.awk" invoice.awk | sha256sum | cut -d ' ' -f 1)
[ "$outside" = a3ebc9616942595970fed7ca0c46160f98604424fd1356de2492bc780f9601bf ] || { rm -rf "$tmp"; echo "outside the three functions"; exit 1; }
# The reference: the same file with the three functions as fixed upstream.
cat >"$tmp/csv_split" <<'END_FUNC'
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
          field = field "\""
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
END_FUNC
cat >"$tmp/round_div" <<'END_FUNC'
function round_div(n, d,   q, r) {
  if (n < 0) return -round_div(-n, d)
  q = int(n / d)
  r = n - q * d
  if (r * 2 >= d) q++
  return q
}
END_FUNC
cat >"$tmp/json_str" <<'END_FUNC'
function json_str(s,   out, i, c, len) {
  out = "\""
  len = length(s)
  for (i = 1; i <= len; i++) {
    c = substr(s, i, 1)
    if (c == "\"") out = out "\\\""
    else if (c == "\\") out = out "\\\\"
    else if (c == "\t") out = out "\\t"
    else if (c == "\r") out = out "\\r"
    else if (c == "\n") out = out "\\n"
    else if (c in CTRL) out = out sprintf("\\u%04x", CTRL[c])
    else out = out c
  }
  return out "\""
}
END_FUNC
awk -v mode=ref -v d="$tmp" -f "$tmp/split.awk" invoice.awk >"$tmp/ref.awk"
[ "$(sha256sum <"$tmp/ref.awk" | cut -d ' ' -f 1)" = c8a65973a74a7b222a0046f0865cdba97681e5be9d2a56c132e2d881adc9f74a ] || { rm -rf "$tmp"; echo "reference"; exit 1; }
# Inputs the prompt does not show.
cat >"$tmp/c1.csv" <<'END_CASE'
invoice,date,customer,region,category,description,qty,unit_price,code
A-100,2026-03-02,Acme Corp,CA,general,Widget,3,19.99,
A-100,2026-03-02,Acme Corp,CA,food,Coffee beans,2,12.50,SAVE10
A-100,2026-03-03,Acme Corp,CA,software,Licence renewal for the long product name suite,1,1200.00,
B-7,2026-02-27,"Acme ""West"" Ltd",NY,clothing,"Jacket, size M",1,89.00,
C-1,2026-04-10,Harbor Cafe,TX,general,Return of chairs,-4,45.00,VOLUME
C-1,2026-04-10,Harbor Cafe,TX,general,Chairs,12,45.00,VOLUME
D-9,2026-05-29,Maple Grocers,IL,food,Cable 3\4 adapter,1,10.00,
E-2,2026-06-15,Juniper Schools,MA,books,Readers,150,7.35,VOLUME
E-3,2026-06-20,Juniper Schools,MA,general,Desks,4,210.00,SCHOOL
END_CASE
cat >"$tmp/c2.csv" <<'END_CASE'
# Quoting.
Q-1,2026-01-05,"""Quoted"" Co",NY,general,"Say ""hi""",1,5.00,
Q-1,2026-01-05,"""Quoted"" Co",NY,general,"""","2",1.00,""
Q-2,2026-01-06,"Comma, Inc.",NY,books,"a ""b"", c",1,10.00,SAVE5
Q-3,2026-01-07,Plain,NY,general,"x""""y",1,1.00,
Q-4,2026-01-08,"Trail ""end""",NY,general,"""""",3,0.99,
Q-5,2026-01-09,"Open,NY,general,never closed,1,1.00,
Q-6,2026-01-10,"A ""|"" B",NY,general,"pipe | and back\slash",1,2.00,
END_CASE
cat >"$tmp/c3.csv" <<'END_CASE'
# Refunds and rounding.
R-1,2026-02-01,Harbor Cafe,CA,general,Return,-1,19.99,
R-1,2026-02-01,Harbor Cafe,CA,general,Sale,1,19.99,
R-2,2026-02-02,Delta Freight,TX,general,Bulk return,-15,1.05,VOLUME
R-2,2026-02-02,Delta Freight,TX,general,Bulk sale,15,1.05,VOLUME
R-3,2026-02-03,Delta Freight,TX,general,Small return,-1,0.30,SAVE5
R-3,2026-02-03,Delta Freight,TX,general,Tiny return,-1,0.08,
R-4,2026-03-04,Vale Pharmacy,MO,medicine,Refund pills,-3,4.10,STAFF
R-4,2026-03-04,Vale Pharmacy,MO,food,Refund snacks,-7,1.13,PRICEMATCH
R-5,2026-03-05,Vale Pharmacy,MO,general,Big refund,-120,2.37,VOLUME
R-6,2026-04-06,Zephyr Cycles,WA,general,Return frame,-1,999.99,CLEARANCE
END_CASE
cat >"$tmp/c4.csv" <<'END_CASE'
J-1,2026-05-01,Indigo Studio,OR,general,C:\temp\,1,1.00,
J-1,2026-05-01,Indigo Studio,OR,general,"a\""b",1,1.00,
J-1,2026-05-01,Indigo Studio,OR,general,two \\ slashes,1,1.00,
J-2,2026-05-02,Back\slash Co,OR,services,"end\",2,3.00,
J-3,2026-05-03,"Q ""\"" R",OR,software,"\\server\share",1,4.00,
END_CASE
cat >"$tmp/c5.csv" <<'END_CASE'
invoice,date,customer,region,category,description,qty,unit_price,code
X-1,2026-02-30,Acme Corp,CA,general,Bad date,1,1.00,
X-2,2026-02-28,Acme Corp,ZZ,general,Bad region,1,1.00,
X-3,2026-02-28,Acme Corp,CA,general,Zero qty,0,1.00,
X-4,2026-02-28,Acme Corp,CA,general,Bad price,1,1.234,
X-5,2026-02-28,Acme Corp,CA,general,Bad code,1,1.00,NOPE
X-6,2026-02-28,Acme Corp,CA,general,Fine,1,1.00,
X-6,2026-02-28,Other Corp,CA,general,Other customer,1,1.00,
X-7,2026-02-28,Acme Corp,CA,general,Too,many,1,1.00,,
X-8,2026-02-28,"Acme ""Q"" Corp",CA,general,"Unclosed ""quote,1,1.00,
END_CASE
cat >"$tmp/c6.csv" <<'END_CASE'
invoice,date,customer,region,category,description,qty,unit_price,code
H-1,2026-11-25,Delta Freight,FL,general,Holiday due,1,100.00,
H-2,2028-02-29,Delta Freight,FL,general,Leap day,2,50.00,SAVE20
H-3,2027-12-31,Harbor Cafe,GA,general,Year end,1,10.00,
H-4,2026-06-30,Umber Paints,NC,food,Lunch,9,8.88,
H-5,2026-07-01,Umber Paints,NC,general,"Paint, ""red""",64,12.34,VOLUME
H-6,2026-01-15,Umber Paints,NC,general,January,1,1.00,
H-6,2026-01-10,Umber Paints,NC,general,Earlier line,1,1.00,
END_CASE
cat >"$tmp/c7.csv" <<'END_CASE'
invoice,date,customer,region,category,description,qty,unit_price,code
A-1,2026-03-02,Acme Corp,CA,general,Widget,3,19.99,
A-1,2026-03-02,Acme Corp,CA,food,Coffee beans,2,12.50,SAVE10
B-2,2026-03-05,"Birch & Daughters",TX,general,"Chairs, oak",12,45.00,VOLUME
A-3,2026-03-03,Acme Corp,CA,general,Widget (returned),-1,19.99,
END_CASE
printf 'T-1,2026-05-04,Tab Co,OR,general,tab\there,1,1.00,\n' >>"$tmp/c4.csv"
printf 'T-2,2026-05-04,"Crlf ""Co""",CA,general,crlf line,-1,0.10,\r\n' >>"$tmp/c4.csv"
# Every report on every input, against the reference: stdout, stderr and the
# exit status.
run() {
  awk -f "$1" -v report="$2" -v today=2026-07-01 "$3" >"$4.out" 2>"$4.err" </dev/null
  echo "$?" >>"$4.out"
}
for c in 1 2 3 4 5 6 7; do
  for r in invoice summary monthly tax aging statement customers markdown csv json; do
    run invoice.awk "$r" "$tmp/c$c.csv" "$tmp/got"
    run "$tmp/ref.awk" "$r" "$tmp/c$c.csv" "$tmp/want"
    if ! cmp -s "$tmp/got.out" "$tmp/want.out" || ! cmp -s "$tmp/got.err" "$tmp/want.err"; then
      rm -rf "$tmp"
      echo "case $c $r"
      exit 1
    fi
  done
done
rm -rf "$tmp"
exit 0
