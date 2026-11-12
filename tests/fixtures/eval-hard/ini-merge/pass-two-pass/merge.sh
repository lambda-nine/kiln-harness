#!/bin/sh
# merge.sh FILE...: one awk per file turns its lines into tab-separated
# records (or reports its first error); a last awk merges the records.
LC_ALL=C
export LC_ALL
[ "$#" -gt 0 ] || exit 2
records=
for f in "$@"; do
  [ -f "$f" ] && [ -r "$f" ] || exit 2
  part=$(FILE=$f awk '
    { line = $0 }
    line ~ /^[ \t]*([;#].*)?$/ { next }
    match(line, /^[ \t]*\[[ \t]*[A-Za-z0-9._-]+[ \t]*\][ \t]*$/) {
      gsub(/[][ \t]/, "", line)
      section = tolower(line)
      printf "S\t%s\n", section
      next
    }
    match(line, /^[ \t]*[A-Za-z0-9._-]+[ \t]*=/) {
      key = substr(line, 1, RLENGTH - 1)
      gsub(/[ \t]/, "", key)
      value = substr(line, RLENGTH + 1)
      sub(/^[ \t]+/, "", value)
      sub(/[ \t]+$/, "", value)
      printf "K\t%s\t%s\t%s\n", section, tolower(key), value
      next
    }
    { printf "E\t%s:%d: malformed\n", ENVIRON["FILE"], FNR; exit }' "$f")
  # Only a record whose tag is E is an error; a value may hold E<TAB>.
  err=$(printf '%s\n' "$part" | grep '^E	' | cut -f 2-)
  if [ -n "$err" ]; then
    printf '%s\n' "$err" >&2
    exit 3
  fi
  records="$records$part
"
done
printf '%s' "$records" | awk -F '\t' '
$1 == "S" { if (!($2 in seen)) { seen[$2] = 1; name[++ns] = $2 } }
$1 == "K" {
  s = $2; k = $3
  v = $0
  for (i = 1; i <= 3; i++) v = substr(v, index(v, "\t") + 1)
  if (!((s, k) in pos)) { pos[s, k] = ++nk[s]; order[s, nk[s]] = k }
  if (v == "!unset") delete val[s, k]
  else val[s, k] = v
}
function group(s,    j, body) {
  body = ""
  for (j = 1; j <= nk[s]; j++) if ((s, order[s, j]) in val) body = body order[s, j] "=" val[s, order[s, j]] "\n"
  if (body == "") return
  printf "%s%s%s", sep, (s == "" ? "" : "[" s "]\n"), body
  sep = "\n"
}
END {
  sep = ""
  group("")
  for (i = 1; i <= ns; i++) if (name[i] != "") group(name[i])
}'
