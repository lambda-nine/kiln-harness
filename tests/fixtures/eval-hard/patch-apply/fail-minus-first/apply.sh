#!/bin/sh
# apply.sh PATCH: apply a unified diff to /workspace/tree, all or nothing
# (docs/patch.md).
# The tree sits next to this script: /workspace/tree.
TREE=$(cd "$(dirname "$0")" && pwd)/tree || exit 2
{ [ -f "$1" ] && [ -r "$1" ]; } || { echo "cannot read $1" >&2; exit 2; }
stage=$(mktemp -d) || exit 2
trap 'rm -rf "$stage"' EXIT
trap 'exit 2' INT TERM HUP
PATCH=$1 TREE=$TREE STAGE=$stage awk '
# shq(s): s in single quotes for sh, each quote in it written as
# quote, backslash, quote, quote.
function shq(s,    q, n, part, i, out) {
  q = sprintf("%c", 39)
  n = split(s, part, q)
  out = q part[1]
  for (i = 2; i <= n; i++) out = out q "\\" q q part[i]
  return out q
}
function say(msg, code) {
  print msg | "cat 1>&2"
  close("cat 1>&2")
  status = code
  exit code
}
function hpath(s) {
  s = substr(s, 5)
  if (index(s, "\t")) s = substr(s, 1, index(s, "\t") - 1)
  if (s == "/dev/null") return s
  if (!index(s, "/")) return ""
  return substr(s, index(s, "/") + 1)
}
BEGIN {
  n = 0
  while ((r = (getline line < ENVIRON["PATCH"])) > 0) P[++n] = line
  if (r < 0) say("cannot read " ENVIRON["PATCH"], 2)

  # Parse. Sections: kind[s], path[s], nh[s]; hunks h of section s:
  # os[s,h] old start, oc[s,h] old count, nc[s,h] new count,
  # body lines b = 1..nb[s,h]: bt[s,h,b] type, bx[s,h,b] text, be[s,h,b]
  # 1 when the line has no LF.
  ns = 0
  i = 1
  while (i <= n) {
    s = P[i]
    if (substr(s, 1, 4) == "--- ") {
      if (i == n || substr(P[i + 1], 1, 4) != "+++ ") say("malformed patch at line " i, 2)
      old = hpath(s)
      new = hpath(P[i + 1])
      if (old == "" || new == "" || (old == "/dev/null" && new == "/dev/null"))
        say("malformed patch at line " (i + 1), 2)
      ns++
      nh[ns] = 0
      if (old == "/dev/null") { kind[ns] = "created"; path[ns] = new }
      else if (new == "/dev/null") { kind[ns] = "deleted"; path[ns] = old }
      else { kind[ns] = "patched"; path[ns] = new }
      i += 2
      continue
    }
    if (substr(s, 1, 4) == "+++ ") say("malformed patch at line " i, 2)
    if (substr(s, 1, 2) == "@@") {
      if (ns == 0 || s !~ /^@@ -[0-9]+(,[0-9]+)? [+][0-9]+(,[0-9]+)? @@/)
        say("malformed patch at line " i, 2)
      hdr = i
      split(substr(s, 5), f, " ")
      a = f[1]
      b = substr(f[2], 2)
      h = ++nh[ns]
      os[ns, h] = a + 0
      oc[ns, h] = index(a, ",") ? substr(a, index(a, ",") + 1) + 0 : 1
      nc[ns, h] = index(b, ",") ? substr(b, index(b, ",") + 1) + 0 : 1
      if (index(a, ",")) os[ns, h] = substr(a, 1, index(a, ",") - 1) + 0
      no = nn = 0
      k = 0
      # pb: the previous patch line was a body line, so a marker may follow.
      pb = 0
      i++
      while (no < oc[ns, h] || nn < nc[ns, h]) {
        if (i > n) say("malformed patch at line " hdr, 2)
        c = substr(P[i], 1, 1)
        if (c == "\\") {
          if (!pb) say("malformed patch at line " i, 2)
          be[ns, h, k] = 1
          pb = 0
          i++
          continue
        }
        if (c == " ") { if (no >= oc[ns, h] || nn >= nc[ns, h]) say("malformed patch at line " i, 2); no++; nn++ }
        else if (c == "-") { if (no >= oc[ns, h]) say("malformed patch at line " i, 2); no++ }
        else if (c == "+") { if (nn >= nc[ns, h]) say("malformed patch at line " i, 2); nn++ }
        else say("malformed patch at line " i, 2)
        k++
        bt[ns, h, k] = c
        bx[ns, h, k] = substr(P[i], 2)
        be[ns, h, k] = 0
        pb = 1
        i++
      }
      if (i <= n && substr(P[i], 1, 1) == "\\") {
        if (!pb) say("malformed patch at line " i, 2)
        be[ns, h, k] = 1
        i++
      }
      nb[ns, h] = k
      continue
    }
    c = substr(s, 1, 1)
    if (c == " " || c == "-" || c == "+" || c == "\\") say("malformed patch at line " i, 2)
    i++
  }

  # Apply each section in memory; write results to the staging directory.
  for (s = 1; s <= ns; s++) {
    full = ENVIRON["TREE"] "/" path[s]
    exists = system("test -f " shq(full)) == 0
    present = system("test -e " shq(full)) == 0
    if (kind[s] == "created" ? present : !exists) say("cannot patch " path[s], 1)
    m = 0
    if (kind[s] != "created") {
      while ((getline line < full) > 0) { m++; T[m] = line; E[m] = 0 }
      close(full)
      if (m > 0 && system("test -n \"$(tail -c 1 " shq(full) ")\"") == 0) E[m] = 1
    }
    shift = 0
    for (h = 1; h <= nh[s]; h++) {
      # Old and new lines.
      no = nn = 0
      for (k = 1; k <= nb[s, h]; k++) {
        t = bt[s, h, k]
        if (t != "+") { no++; OX[no] = bx[s, h, k]; OE[no] = be[s, h, k] }
        if (t != "-") { nn++; NX[nn] = bx[s, h, k]; NE[nn] = be[s, h, k] }
      }
      base = os[s, h] + (no == 0 ? 1 : 0)
      e = base + shift
      last = m - no + 1
      at = 0
      for (d = 0; !at && (e + d <= last || e - d >= 1); d++) {
        if (d > 0 && e - d >= 1 && e - d <= last && match_at(e - d)) at = e - d
        else if (e + d >= 1 && e + d <= last && match_at(e + d)) at = e + d
      }
      if (!at) say("hunk " h " of " path[s] " failed", 1)
      # Replace lines at..at+no-1 with the new lines.
      split("", U); split("", UE)
      u = 0
      for (j = 1; j < at; j++) { u++; U[u] = T[j]; UE[u] = E[j] }
      for (j = 1; j <= nn; j++) { u++; U[u] = NX[j]; UE[u] = NE[j] }
      for (j = at + no; j <= m; j++) { u++; U[u] = T[j]; UE[u] = E[j] }
      m = u
      for (j = 1; j <= m; j++) { T[j] = U[j]; E[j] = UE[j] }
      shift = at - base + nn - no
    }
    if (kind[s] == "deleted") {
      if (m > 0) say("cannot patch " path[s], 1)
      plan[s] = ""
      continue
    }
    out = ENVIRON["STAGE"] "/" s
    printf "" > out
    for (j = 1; j <= m; j++) printf "%s%s", T[j], (E[j] ? "" : "\n") > out
    close(out)
  }
  for (s = 1; s <= ns; s++) printf "%s\t%s\t%s\n", kind[s], ENVIRON["STAGE"] "/" s, path[s]
  exit 0
}
function match_at(p,    j) {
  for (j = 1; j <= no; j++)
    if (T[p + j - 1] != OX[j] || E[p + j - 1] != OE[j]) return 0
  return 1
}
' >"$stage/plan" || exit $?
tab=$(printf '\t')
while IFS=$tab read -r kind file name; do
  case $kind in
    created)
      mkdir -p "$TREE/$(dirname "$name")" && cat "$file" >"$TREE/$name" || exit 2 ;;
    patched)
      cat "$file" >"$TREE/$name" || exit 2 ;;
    deleted)
      rm -f "$TREE/$name" || exit 2 ;;
  esac
done <"$stage/plan"
while IFS=$tab read -r kind file name; do
  echo "$kind $name"
done <"$stage/plan"
