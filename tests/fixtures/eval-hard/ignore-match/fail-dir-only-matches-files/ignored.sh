#!/bin/sh
# ignored.sh RULES: print the paths on stdin that RULES ignores
# (docs/ignore-rules.md).
{ [ -f "$1" ] && [ -r "$1" ]; } || exit 2
RULES=$1 LC_ALL=C exec awk '
function bad(n) {
  print ENVIRON["RULES"] ":" n ": bad pattern" | "cat 1>&2"
  close("cat 1>&2")
  exit 3
}
# parse(n, raw): add rule nr+1 from line n, or skip the line.
function parse(n, raw,    u, e, len, i, c, k, j, s, start, stop) {
  # Units: u[i] is a character, e[i] is 1 when it was escaped.
  len = 0
  for (i = 1; i <= length(raw); i++) {
    c = substr(raw, i, 1)
    if (c == "\\") {
      if (i == length(raw)) { len++; u[len] = ""; e[len] = 2; break }
      i++
      len++; u[len] = substr(raw, i, 1); e[len] = 1
      if (u[len] == "/") e[len] = 0
      continue
    }
    len++; u[len] = c; e[len] = 0
  }
  while (len > 0 && u[len] == " " && !e[len]) len--
  if (len == 0) return
  if (u[1] == "#" && !e[1]) return
  if (e[len] == 2) bad(n)
  start = 1
  stop = len
  nr++
  neg[nr] = 0
  if (u[1] == "!" && !e[1]) { neg[nr] = 1; start++ }
  dir[nr] = 0
  if (stop >= start && u[stop] == "/" && !e[stop]) { dir[nr] = 1; stop-- }
  anch[nr] = 0
  for (i = start; i <= stop; i++) if (u[i] == "/" && !e[i]) anch[nr] = 1
  if (anch[nr] && u[start] == "/" && !e[start]) start++
  if (start > stop) bad(n)
  # Split into pattern segments of tokens.
  s = 1
  k = 0
  raw = 0
  for (i = start; i <= stop + 1; i++) {
    if (i > stop || (u[i] == "/" && !e[i])) {
      nt[nr, s] = k
      glob[nr, s] = raw == 2 && k == 1 && tt[nr, s, 1] == "**"
      if (i <= stop) { s++; k = 0; raw = 0 }
      continue
    }
    raw++
    c = u[i]
    if (!e[i] && c == "*") {
      if (k > 0 && (tt[nr, s, k] == "*" || tt[nr, s, k] == "**")) tt[nr, s, k] = "**"
      else { k++; tt[nr, s, k] = "*" }
      continue
    }
    k++
    if (!e[i] && c == "?") { tt[nr, s, k] = "?"; continue }
    if (!e[i] && c == "[") {
      i++
      cneg = 0
      if (i <= stop && u[i] == "!" && !e[i]) { cneg = 1; i++ }
      mem = ""
      first = 1
      while (1) {
        if (i > stop || (u[i] == "/" && !e[i])) bad(n)
        if (u[i] == "]" && !e[i] && !first) break
        lo = u[i]
        hi = lo
        if (i + 2 <= stop && u[i + 1] == "-" && !e[i + 1] && !(u[i + 2] == "]" && !e[i + 2]) && !(u[i + 2] == "/" && !e[i + 2])) {
          hi = u[i + 2]
          i += 2
        }
        mem = mem lo hi
        first = 0
        i++
      }
      tt[nr, s, k] = cneg ? "!" : "["
      tv[nr, s, k] = mem
      continue
    }
    tt[nr, s, k] = "="
    tv[nr, s, k] = c
  }
  ns[nr] = s
  # A lone ** among several segments is a run of segments; otherwise a *.
  for (j = 1; j <= s; j++) {
    if (glob[nr, j] && s == 1) glob[nr, j] = 0
    for (k = 1; k <= nt[nr, j]; k++) if (tt[nr, j, k] == "**") tt[nr, j, k] = "*"
  }
}
function inclass(mem, c,    p) {
  for (p = 1; p < length(mem); p += 2)
    if (c >= substr(mem, p, 1) && c <= substr(mem, p + 1, 1)) return 1
  return 0
}
# seg(r, s, k, str, i): tokens k.. of segment s of rule r match str from i.
function seg(r, s, k, str, i,    n, t, c, j) {
  n = nt[r, s]
  while (k <= n) {
    t = tt[r, s, k]
    if (t == "*") {
      for (j = i; j <= length(str) + 1; j++) if (seg(r, s, k + 1, str, j)) return 1
      return 0
    }
    if (i > length(str)) return 0
    c = substr(str, i, 1)
    if (t == "=" && c != tv[r, s, k]) return 0
    if (t == "[" && !inclass(tv[r, s, k], c)) return 0
    if (t == "!" && inclass(tv[r, s, k], c)) return 0
    k++
    i++
  }
  return i > length(str)
}
# segs(r, s, j): segments s.. of rule r match path segments j..depth.
function segs(r, s, j,    jj) {
  if (s > ns[r]) return j > depth
  if (glob[r, s]) {
    if (s == ns[r]) return j <= depth
    for (jj = j; jj <= depth + 1; jj++) if (segs(r, s + 1, jj)) return 1
    return 0
  }
  if (j > depth) return 0
  return seg(r, s, 1, ps[j], 1) && segs(r, s + 1, j + 1)
}
function excluded(isdir,    r) {
  for (r = nr; r >= 1; r--) {
    if (anch[r] ? segs(r, 1, 1) : seg(r, 1, 1, ps[depth], 1)) return !neg[r]
  }
  return 0
}
BEGIN {
  f = ENVIRON["RULES"]
  n = 0
  while ((got = (getline line < f)) > 0) parse(++n, line)
  if (got < 0) exit 2
  close(f)
}
{
  p = $0
  isdir = p ~ /\/$/
  m = split(isdir ? substr(p, 1, length(p) - 1) : p, ps, "/")
  for (depth = 1; depth <= m; depth++) {
    if (excluded(depth < m || isdir)) { print p; break }
  }
}
'
