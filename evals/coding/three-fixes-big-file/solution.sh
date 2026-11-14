cd /workspace || exit 1
tmp=$(mktemp -d) || exit 1
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
awk -v d="$tmp" '
/^function (csv_split|round_div|json_str)[(]/ {
  name = $2
  sub(/[(].*/, "", name)
  f = d "/" name
  while ((getline l < f) > 0) print l
  close(f)
  done++
  skip = 1
  next
}
skip { if ($0 == "}") skip = 0; next }
{ print }
END { exit (done != 3) }
' invoice.awk >"$tmp/invoice.awk" || { rm -rf "$tmp"; exit 1; }
cat "$tmp/invoice.awk" >invoice.awk
rm -rf "$tmp"
