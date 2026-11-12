#!/bin/sh
# stak.sh FILE: check and run a Stak program (docs/stak.md).
{ [ -f "$1" ] && [ -r "$1" ]; } || exit 2
exec awk '
function syntax(line) {
  print "error: syntax at line " line | "cat 1>&2"
  close("cat 1>&2")
  exit 2
}
function fail(msg, line) {
  print "error: " msg " at line " line | "cat 1>&2"
  close("cat 1>&2")
  exit 3
}
function emit_op(o, a, line) {
  nc++
  op[nc] = o
  av[nc] = a
  al[nc] = line
  return nc
}
function fmt(v) {
  return v == 0 ? "0" : sprintf("%.0f", v)
}
{ src[++nl] = $0 }
END {
  # Built-in words and their opcodes; control words are 0.
  split("+ - * / mod dup drop swap over rot = < > not . cr emit", names, " ")
  for (i = 1; i in names; i++) builtin[names[i]] = 9 + i
  split("if else then begin while repeat until : ;", names, " ")
  for (i = 1; i in names; i++) builtin[names[i]] = 0

  # Tokens: kind W (word or number), S (string) or X (unterminated string).
  nt = 0
  for (ln = 1; ln <= nl; ln++) {
    s = src[ln]
    len = length(s)
    i = 1
    while (i <= len) {
      c = substr(s, i, 1)
      if (c == " " || c == "\t") { i++; continue }
      j = i
      while (j <= len) {
        c = substr(s, j, 1)
        if (c == " " || c == "\t") break
        j++
      }
      t = substr(s, i, j - i)
      if (t == "#") break
      nt++
      tl[nt] = ln
      if (t == ".\"") {
        k = (j > len) ? 0 : index(substr(s, j + 1), "\"")
        if (k == 0) { tk[nt] = "X"; break }
        tk[nt] = "S"
        tv[nt] = substr(s, j + 1, k - 1)
        i = j + 1 + k
        continue
      }
      tk[nt] = "W"
      tv[nt] = t
      i = j
    }
  }

  # Check and compile in one pass, in file order.
  nc = 0
  d = 0
  for (t = 1; t <= nt; t++) {
    line = tl[t]
    if (tk[t] == "X") syntax(line)
    if (tk[t] == "S") { emit_op(2, tv[t], line); continue }
    w = tv[t]
    if (w ~ /^-?[0-9]+$/) { emit_op(1, w + 0, line); continue }
    if (!(w in builtin)) {
      if (w in dict) emit_op(3, dict[w], line)
      else fail("unknown word " w, line)
      continue
    }
    if (builtin[w] > 0) { emit_op(builtin[w], "", line); continue }
    if (w == ":") {
      if (d > 0) syntax(line)
      if (t == nt) syntax(line)
      t++
      if (tk[t] != "W" || tv[t] ~ /^-?[0-9]+$/ || (tv[t] in builtin)) syntax(tl[t])
      d = 1
      ct[d] = "def"
      cl[d] = line
      cx[d] = emit_op(5, 0, line)
      dict[tv[t]] = nc + 1
    } else if (w == ";") {
      if (d != 1 || ct[d] != "def") syntax(line)
      emit_op(4, "", line)
      av[cx[d]] = nc + 1
      d--
    } else if (w == "if") {
      d++
      ct[d] = "if"
      cl[d] = line
      cy[d] = 0
      cx[d] = emit_op(6, 0, line)
    } else if (w == "else") {
      if (d == 0 || ct[d] != "if" || cy[d]) syntax(line)
      j = emit_op(5, 0, line)
      av[cx[d]] = nc + 1
      cx[d] = j
      cy[d] = 1
    } else if (w == "then") {
      if (d == 0 || ct[d] != "if") syntax(line)
      av[cx[d]] = nc + 1
      d--
    } else if (w == "begin") {
      d++
      ct[d] = "begin"
      cl[d] = line
      cy[d] = 0
      cb[d] = nc + 1
    } else if (w == "while") {
      if (d == 0 || ct[d] != "begin" || cy[d]) syntax(line)
      cx[d] = emit_op(6, 0, line)
      cy[d] = 1
    } else if (w == "repeat") {
      if (d == 0 || ct[d] != "begin" || !cy[d]) syntax(line)
      emit_op(5, cb[d], line)
      av[cx[d]] = nc + 1
      d--
    } else if (w == "until") {
      if (d == 0 || ct[d] != "begin" || cy[d]) syntax(line)
      emit_op(7, cb[d], line)
      d--
    }
  }
  if (d > 0) syntax(cl[d])

  # Run.
  sp = 0
  rp = 0
  pc = 1
  while (pc <= nc) {
    o = op[pc]
    if (o == 1) { st[++sp] = av[pc]; pc++; continue }
    if (o == 6) {
      if (sp < 1) fail("stack underflow", al[pc])
      if (st[sp--] == 0) pc = av[pc]
      else pc++
      continue
    }
    if (o == 5) { pc = av[pc]; continue }
    if (o == 7) {
      if (sp < 1) fail("stack underflow", al[pc])
      if (st[sp--] == 0) pc = av[pc]
      else pc++
      continue
    }
    if (o == 3) { rs[++rp] = pc + 1; pc = av[pc]; continue }
    if (o == 4) { pc = rs[rp--]; continue }
    if (o == 2) { printf "%s", av[pc]; pc++; continue }
    if (o == 8) fail("unknown word " av[pc], al[pc])
    if (o <= 14 || (o >= 20 && o <= 22)) {
      if (sp < 2) fail("stack underflow", al[pc])
      b = st[sp--]
      a = st[sp]
      if (o == 10) a += b
      else if (o == 11) a -= b
      else if (o == 12) a *= b
      else if (o == 13 || o == 14) {
        if (b == 0) fail("division by zero", al[pc])
        q = int(a / b)
        a = o == 13 ? q : a - b * q
      }
      else if (o == 20) a = a == b
      else if (o == 21) a = a < b
      else a = a > b
      st[sp] = a
      pc++
      continue
    }
    if (o == 15) {
      if (sp < 1) fail("stack underflow", al[pc])
      st[sp + 1] = st[sp]
      sp++
    } else if (o == 16) {
      if (sp < 1) fail("stack underflow", al[pc])
      sp--
    } else if (o == 17) {
      if (sp < 2) fail("stack underflow", al[pc])
      a = st[sp]
      st[sp] = st[sp - 1]
      st[sp - 1] = a
    } else if (o == 18) {
      if (sp < 2) fail("stack underflow", al[pc])
      st[sp + 1] = st[sp - 1]
      sp++
    } else if (o == 19) {
      if (sp < 3) fail("stack underflow", al[pc])
      a = st[sp - 2]
      st[sp - 2] = st[sp - 1]
      st[sp - 1] = st[sp]
      st[sp] = a
    } else if (o == 23) {
      if (sp < 1) fail("stack underflow", al[pc])
      st[sp] = st[sp] == 0
    } else if (o == 24) {
      if (sp < 1) fail("stack underflow", al[pc])
      printf "%s ", fmt(st[sp--])
    } else if (o == 25) {
      printf "\n"
    } else if (o == 26) {
      if (sp < 1) fail("stack underflow", al[pc])
      a = st[sp]
      if (a < 32 || a > 126) fail("bad emit", al[pc])
      sp--
      printf "%c", a
    }
    pc++
  }
  exit 0
}' <"$1"
