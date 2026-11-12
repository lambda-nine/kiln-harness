#!/bin/sh
# A Stak interpreter that walks the token list directly, with each control
# word's partner and each word's binding worked out before the run.
test -f "$1" || exit 2
test -r "$1" || exit 2
awk '
function die(code, text) {
  print text | "cat 1>&2"
  close("cat 1>&2")
  exit code
}
function isnum(x) { return x ~ /^-?[0-9]+$/ }
function need(k) { if (top < k) die(3, "error: stack underflow at line " line[ip]) }
{
  rest = $0
  while (match(rest, /[^ \t]+/)) {
    word = substr(rest, RSTART, RLENGTH)
    rest = substr(rest, RSTART + RLENGTH)
    if (word == "#") break
    n++
    line[n] = NR
    if (word != ".\"") { tok[n] = word; continue }
    # rest now starts with the blank after ."; the text follows it.
    q = rest == "" ? 0 : index(substr(rest, 2), "\"")
    if (q == 0) { tok[n] = "BAD"; kind[n] = "bad"; break }
    tok[n] = substr(rest, 2, q - 1)
    kind[n] = "str"
    rest = substr(rest, q + 2)
  }
}
END {
  nb = split("+ - * / mod dup drop swap over rot = < > not . cr emit if else then begin while repeat until : ;", b, " ")
  for (i = 1; i <= nb; i++) isbuiltin[b[i]] = 1

  # Static pass: partners for control words, bindings for words.
  depth = 0
  indef = 0
  for (i = 1; i <= n; i++) {
    if (kind[i] == "bad") die(2, "error: syntax at line " line[i])
    if (kind[i] == "str") continue
    w = tok[i]
    if (isnum(w)) { kind[i] = "num"; val[i] = w + 0; continue }
    if (!(w in isbuiltin)) {
      kind[i] = "call"
      if (w in current) target[i] = current[w]
      else kind[i] = "unknown"
      continue
    }
    kind[i] = w
    if (w == ":") {
      if (depth > 0) die(2, "error: syntax at line " line[i])
      if (i == n) die(2, "error: syntax at line " line[i])
      if (kind[i + 1] != "" || isnum(tok[i + 1]) || (tok[i + 1] in isbuiltin))
        die(2, "error: syntax at line " line[i + 1])
      current[tok[i + 1]] = i + 2
      kind[i + 1] = "name"
      open[++depth] = i
      i++
    } else if (w == "if" || w == "begin") {
      open[++depth] = i
    } else if (w == ";" || w == "else" || w == "then" || w == "while" || w == "repeat" || w == "until") {
      o = depth > 0 ? open[depth] : 0
      ok = o > 0
      if (w == ";") ok = ok && kind[o] == ":"
      else if (w == "else") ok = ok && kind[o] == "if" && !(o in mid)
      else if (w == "then") ok = ok && kind[o] == "if"
      else if (w == "while") ok = ok && kind[o] == "begin" && !(o in mid)
      else if (w == "repeat") ok = ok && kind[o] == "begin" && (o in mid)
      else if (w == "until") ok = ok && kind[o] == "begin" && !(o in mid)
      if (!ok) die(2, "error: syntax at line " line[i])
      if (w == "else" || w == "while") { mid[o] = i; continue }
      depth--
      if (w == ";") { skip[o] = i + 1; continue }
      if (w == "then") {
        if (o in mid) { jump[o] = mid[o] + 1; jump[mid[o]] = i + 1 }
        else jump[o] = i + 1
      } else if (w == "repeat") {
        jump[mid[o]] = i + 1
        jump[i] = o + 1
      } else if (w == "until") {
        jump[i] = o + 1
      }
    }
  }
  if (depth > 0) die(2, "error: syntax at line " line[open[depth]])

  top = 0
  calls = 0
  ip = 1
  while (ip <= n) {
    k = kind[ip]
    if (k == "num") { s[++top] = val[ip]; ip++ }
    else if (k == "call") { ret[++calls] = ip + 1; ip = target[ip] }
    else if (k == ";") ip = ret[calls--]
    else if (k == "if" || k == "while") {
      need(1)
      ip = s[top--] != 0 ? ip + 1 : jump[ip]
    }
    else if (k == "else" || k == "repeat") ip = jump[ip]
    else if (k == "until") {
      need(1)
      ip = s[top--] != 0 ? ip + 1 : jump[ip]
    }
    else if (k == ":") ip = skip[ip]
    else {
      if (k == "str") printf "%s", tok[ip]
      else if (k == "unknown") die(3, "error: unknown word " tok[ip] " at line " line[ip])
      else if (k == "+") { need(2); top--; s[top] = s[top] + s[top + 1] }
      else if (k == "-") { need(2); top--; s[top] = s[top] - s[top + 1] }
      else if (k == "*") { need(2); top--; s[top] = s[top] * s[top + 1] }
      else if (k == "/" || k == "mod") {
        need(2)
        if (s[top] == 0) die(3, "error: division by zero at line " line[ip])
        top--
        x = s[top]; y = s[top + 1]
        r = x % y
        s[top] = k == "mod" ? r : (x - r) / y
      }
      else if (k == "dup") { need(1); s[top + 1] = s[top]; top++ }
      else if (k == "drop") { need(1); top-- }
      else if (k == "swap") { need(2); x = s[top]; s[top] = s[top - 1]; s[top - 1] = x }
      else if (k == "over") { need(2); s[top + 1] = s[top - 1]; top++ }
      else if (k == "rot") { need(3); x = s[top - 2]; s[top - 2] = s[top - 1]; s[top - 1] = s[top]; s[top] = x }
      else if (k == "=") { need(2); top--; s[top] = s[top] == s[top + 1] ? 1 : 0 }
      else if (k == "<") { need(2); top--; s[top] = s[top] < s[top + 1] ? 1 : 0 }
      else if (k == ">") { need(2); top--; s[top] = s[top] > s[top + 1] ? 1 : 0 }
      else if (k == "not") { need(1); s[top] = s[top] == 0 ? 1 : 0 }
      else if (k == ".") {
        need(1)
        x = s[top--]
        if (x == 0) printf "0 "
        else printf "%.0f ", x
      }
      else if (k == "cr") printf "\n"
      else if (k == "emit") {
        need(1)
        if (s[top] < 32 || s[top] > 126) die(3, "error: bad emit at line " line[ip])
        printf "%c", s[top--]
      }
      ip++
    }
  }
}' <"$1"
