#!/bin/sh
# Validate the patch, apply it file by file to a private copy of the tree,
# and only when every file worked, swap the copy in for the tree.
here=$(cd "$(dirname "$0")" && pwd) || exit 2
tree=$here/tree
test -f "$1" && test -r "$1" || exit 2
work=$(mktemp -d "$here/.apply.XXXXXX") || exit 2
trap 'rm -rf "$work"' EXIT
tab=$(printf '\t')

fail() {
  echo "$1" >&2
  exit "$2"
}

# Pass 1: check the whole patch, and split it into $work/list (one line
# per section: number, kind, path) and $work/h.N (the hunks of section N).
awk -v W="$work" '
function strip(p) {
  sub(/\t.*/, "", p)
  if (p == "/dev/null") return p
  if (p !~ /\//) return ""
  sub(/^[^\/]*\//, "", p)
  return p
}
function bad(at) {
  printf "malformed patch at line %d\n", at | "cat 1>&2"
  close("cat 1>&2")
  exit 2
}
{ line[NR] = $0 }
END {
  sec = 0
  state = "out"
  for (i = 1; i <= NR; i++) {
    x = line[i]
    if (state == "body") {
      t = substr(x, 1, 1)
      if (t == "\\") {
        # A marker must follow a body line directly.
        if (!body) bad(i)
        body = 0
        prev = substr(prev, 1, 1) "1" substr(prev, 3)
        continue
      }
      if (t != " " && t != "-" && t != "+") bad(i)
      if (t != "+") oleft--
      if (t != "-") nleft--
      if (oleft < 0 || nleft < 0) bad(i)
      if (any) print prev > out
      prev = t "0" substr(x, 2)
      any = 1
      body = 1
      if (oleft == 0 && nleft == 0) {
        state = "tail"
      }
      continue
    }
    if (state == "tail") {
      state = "out"
      if (substr(x, 1, 1) == "\\") {
        prev = substr(prev, 1, 1) "1" substr(prev, 3)
        print prev > out
        continue
      }
      print prev > out
    }
    if (x ~ /^--- /) {
      if (i == NR || line[i + 1] !~ /^\+\+\+ /) bad(i)
      a = strip(substr(x, 5))
      b = strip(substr(line[i + 1], 5))
      if (a == "" || b == "" || (a == "/dev/null" && b == "/dev/null")) bad(i + 1)
      sec++
      out = W "/h." sec
      printf "" > out
      if (a == "/dev/null") print sec "\tcreated\t" b > (W "/list")
      else if (b == "/dev/null") print sec "\tdeleted\t" a > (W "/list")
      else print sec "\tpatched\t" b > (W "/list")
      i++
      continue
    }
    if (x ~ /^\+\+\+ /) bad(i)
    if (x ~ /^@@/) {
      if (!sec || x !~ /^@@ -[0-9]+(,[0-9]+)? \+[0-9]+(,[0-9]+)? @@/) bad(i)
      split(x, f, " ")
      o = substr(f[2], 2)
      nw = substr(f[3], 2)
      ostart = o; ocount = 1
      if (o ~ /,/) { ostart = substr(o, 1, index(o, ",") - 1); ocount = substr(o, index(o, ",") + 1) }
      ncount = nw ~ /,/ ? substr(nw, index(nw, ",") + 1) : 1
      print "@" (ostart + 0) " " (ocount + 0) " " (ncount + 0) > out
      oleft = ocount + 0
      nleft = ncount + 0
      header = i
      any = 0
      body = 0
      state = (oleft == 0 && nleft == 0) ? "out" : "body"
      continue
    }
    if (x ~ /^[-+ \\]/) bad(i)
  }
  if (state == "body") bad(header)
  if (state == "tail") print prev > out
}' "$1" || exit 2
[ -f "$work/list" ] || : >"$work/list"

# The program that applies one section's hunks to one file.
cat >"$work/apply.awk" <<'AWK'
FILENAME == ARGV[1] {
  if ($0 ~ /^@/) {
    nh++
    split(substr($0, 2), f, " ")
    hs[nh] = f[1]; ho[nh] = f[2]; hn[nh] = f[3]; hb[nh] = 0
  } else {
    hb[nh]++
    bt[nh, hb[nh]] = substr($0, 1, 1)
    bf[nh, hb[nh]] = substr($0, 2, 1)
    bx[nh, hb[nh]] = substr($0, 3)
  }
  next
}
{ m++; L[m] = $0; F[m] = 0 }
END {
  if (m > 0 && !lastnl) F[m] = 1
  shift = 0
  for (h = 1; h <= nh; h++) {
    no = 0; nn = 0
    for (j = 1; j <= hb[h]; j++) {
      if (bt[h, j] != "+") { no++; ol[no] = bx[h, j]; of[no] = bf[h, j] }
      if (bt[h, j] != "-") { nn++; nl[nn] = bx[h, j]; nf[nn] = bf[h, j] }
    }
    start = hs[h] + (ho[h] == 0)
    want = start + shift
    top = m - no + 1
    pos = 0
    for (d = 0; pos == 0; d++) {
      if (want + d > top && want - d < 1) break
      if (fits(want + d)) pos = want + d
      else if (d && fits(want - d)) pos = want - d
    }
    if (!pos) { print h > ENVIRON["KFILE"]; exit 1 }
    # Rebuild: lines before pos, the new lines, the rest.
    c = 0
    for (j = 1; j < pos; j++) { c++; R[c] = L[j]; RF[c] = F[j] }
    for (j = 1; j <= nn; j++) { c++; R[c] = nl[j]; RF[c] = nf[j] }
    for (j = pos + no; j <= m; j++) { c++; R[c] = L[j]; RF[c] = F[j] }
    for (j = 1; j <= c; j++) { L[j] = R[j]; F[j] = RF[j] }
    m = c
    shift = pos - start + nn - no
  }
  for (j = 1; j <= m; j++) printf "%s%s", L[j], (F[j] == 1 ? "" : "\n")
}
function fits(p,    j) {
  if (p < 1 || p > top) return 0
  for (j = 1; j <= no; j++) if (L[p + j - 1] != ol[j] || F[p + j - 1] != of[j]) return 0
  return 1
}
AWK

# Pass 2: apply every section to a copy of the tree.
cp -Rp "$tree" "$work/copy" || exit 2
: >"$work/empty"
while IFS=$tab read -r num kind name; do
  target=$work/copy/$name
  if [ "$kind" = created ]; then
    if [ -e "$target" ] || [ -L "$target" ]; then fail "cannot patch $name" 1; fi
    src=$work/empty
  else
    [ -f "$target" ] || fail "cannot patch $name" 1
    src=$target
  fi
  lastnl=1
  if [ -s "$src" ] && [ "$(tail -c 1 "$src" | wc -l | tr -d ' ')" = 0 ]; then lastnl=0; fi
  if ! KFILE=$work/k awk -v lastnl="$lastnl" -f "$work/apply.awk" "$work/h.$num" "$src" >"$work/out"; then
    fail "hunk $(cat "$work/k") of $name failed" 1
  fi
  case $kind in
    deleted)
      [ -s "$work/out" ] && fail "cannot patch $name" 1
      rm -f "$target" ;;
    created)
      mkdir -p "$(dirname "$target")" && cp "$work/out" "$target" || exit 2 ;;
    patched)
      cat "$work/out" >"$target" || exit 2 ;;
  esac
done <"$work/list"

# Every section worked: swap the copy in.
mv "$tree" "$work/old" && mv "$work/copy" "$tree" || exit 2
while IFS=$tab read -r num kind name; do
  echo "$kind $name"
done <"$work/list"
