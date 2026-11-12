#!/bin/sh
# Translate each rule into an extended regular expression, then test every
# path and each of its ancestor directories against the list.
test -r "$1" && test -f "$1" || exit 2
export LC_ALL=C
F=$1 awk '
function lit(c) {
  if (c ~ /[A-Za-z0-9_]/) return c
  if (c == "\\") return "\\\\"
  if (c == "^") return "\\^"
  return "[" c "]"
}
# class(body, neg): an ERE for one class; body is pairs lo hi.
function class(body, neg,    i, lo, hi, out, close_, caret, dash, rest) {
  close_ = caret = dash = 0
  rest = ""
  for (i = 1; i < length(body); i += 2) {
    lo = substr(body, i, 1)
    hi = substr(body, i + 1, 1)
    if (lo == hi) {
      if (lo == "]") close_ = 1
      else if (lo == "^") caret = 1
      else if (lo == "-") dash = 1
      else if (lo == "\\") rest = rest "\\\\"
      else if (lo != "/") rest = rest lo
      continue
    }
    if (lo > hi) continue
    if (lo <= "/" && hi >= "/") {
      if (lo < "/") rest = rest lo "-."
      if (hi > "/") rest = rest "0-" hi
      continue
    }
    rest = rest lo "-" hi
  }
  # A literal ] must come first in the class, right after the ^.
  if (neg) return "[^" (close_ ? "]" : "") "/" rest (caret ? "^" : "") (dash ? "-" : "") "]"
  if (!close_ && rest == "" && !dash) return caret ? "\\^" : "\001\002\001"
  out = ""
  if (close_ || rest != "" || dash)
    out = "[" (close_ ? "]" : "") rest (caret ? "^" : "") (dash ? "-" : "") "]"
  return out
}
function invalid() {
  printf "%s:%d: bad pattern\n", ENVIRON["F"], lineno | "cat 1>&2"
  close("cat 1>&2")
  exit 3
}
BEGIN {
  F = ENVIRON["F"]
  n = 0
  lineno = 0
  while ((r = getline raw < F) > 0) {
    lineno++
    # Mark escaped characters with a leading \001 so later steps see them.
    s = ""
    for (i = 1; i <= length(raw); i++) {
      c = substr(raw, i, 1)
      if (c != "\\") { s = s c; continue }
      if (i == length(raw)) { s = s "\002"; break }
      i++
      c = substr(raw, i, 1)
      s = s (c == "/" ? "/" : "\001" c)
    }
    while (s ~ / $/ && s !~ /\001 $/) s = substr(s, 1, length(s) - 1)
    if (s == "" || s ~ /^#/) continue
    if (s ~ /\002$/) invalid()
    negate = 0
    if (s ~ /^!/) { negate = 1; s = substr(s, 2) }
    dironly = 0
    if (s ~ /\/$/) { dironly = 1; s = substr(s, 1, length(s) - 1) }
    anchored = index(s, "/") > 0
    if (anchored && s ~ /^\//) s = substr(s, 2)
    if (s == "") invalid()
    nseg = split(s, seg, "/")
    re = ""
    joined = 0
    for (k = 1; k <= nseg; k++) {
      p = seg[k]
      if (p == "**" && nseg > 1) {
        if (k == 1) { re = "([^/]+/)*"; joined = 1; continue }
        # After another ** the separator is already in re.
        if (k == nseg) { re = re (joined ? "" : "/") ".+"; continue }
        re = re (joined ? "" : "/") "([^/]+/)*"
        joined = 1
        continue
      }
      if (k > 1 && !joined) re = re "/"
      joined = 0
      for (i = 1; i <= length(p); i++) {
        c = substr(p, i, 1)
        if (c == "\001") { i++; re = re lit(substr(p, i, 1)); continue }
        if (c == "*") {
          while (substr(p, i + 1, 1) == "*") i++
          re = re "[^/]*"
          continue
        }
        if (c == "?") { re = re "[^/]"; continue }
        if (c != "[") { re = re lit(c); continue }
        # A class.
        j = i + 1
        neg = 0
        if (substr(p, j, 1) == "!") { neg = 1; j++ }
        body = ""
        first = 1
        while (1) {
          if (j > length(p)) invalid()
          d = substr(p, j, 1)
          if (d == "\001") { j++; d = substr(p, j, 1) }
          else if (d == "]" && !first) break
          hi = d
          e = substr(p, j + 1, 1)
          if (e == "-" && j + 2 <= length(p) && substr(p, j + 2, 1) != "]") {
            hi = substr(p, j + 2, 1)
            if (hi == "\001") { hi = substr(p, j + 3, 1); j++ }
            j += 2
          }
          body = body d hi
          first = 0
          j++
        }
        re = re class(body, neg)
        i = j
      }
    }
    n++
    R[n] = "^" re "$"
    N[n] = negate
    D[n] = dironly
    A[n] = anchored
  }
  if (r < 0) exit 2
}
function judge(path, name, isdir,    k) {
  for (k = n; k > 0; k--) {
    if (D[k] && !isdir) continue
    if ((A[k] ? path : name) ~ R[k]) return N[k] ? 0 : 1
  }
  return 0
}
{
  line = $0
  full = line
  isdir = 0
  if (full ~ /\/$/) { isdir = 1; full = substr(full, 1, length(full) - 1) }
  cnt = split(full, part, "/")
  sofar = ""
  out = 0
  for (k = 1; k <= cnt && !out; k++) {
    sofar = k == 1 ? part[1] : sofar "/" part[k]
    if (judge(sofar, part[k], k < cnt || isdir)) out = 1
  }
  if (out) print line
}
'
