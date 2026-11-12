cat > /workspace/order.sh <<'SCRIPT'
#!/bin/sh
# order.sh FILE: print every name so that each follows all of its
# dependencies, taking the byte-wise smallest ready name first.
LC_ALL=C
export LC_ALL
exec awk '
/^[ \t]*$/ || /^[ \t]*#/ { next }
!/^[ \t]*[A-Za-z0-9._-]+[ \t]*:[ \t]*([A-Za-z0-9._-]+([ \t]+[A-Za-z0-9._-]+)*)?[ \t]*$/ {
  printf "syntax error on line %d\n", NR | "cat 1>&2"
  close("cat 1>&2")
  failed = 1
  exit 2
}
{
  p = index($0, ":")
  target = substr($0, 1, p - 1)
  gsub(/[ \t]/, "", target)
  add(target)
  n = split(substr($0, p + 1), deps)
  for (i = 1; i <= n; i++) {
    add(deps[i])
    if (!((target, deps[i]) in EDGE)) {
      EDGE[target, deps[i]] = 1
      WAITING[target]++
      USERS[deps[i]] = USERS[deps[i]] " " target
    }
  }
}
function add(name) {
  if (!(name in KNOWN)) { KNOWN[name] = 1; NAMES[++count] = name; WAITING[name] += 0 }
}
END {
  if (failed) exit 2
  out = ""
  for (done = 0; done < count; done++) {
    best = ""
    for (i = 1; i <= count; i++) {
      name = NAMES[i] ""
      if (!(name in EMITTED) && WAITING[name] == 0 && (best == "" || name < best)) best = name
    }
    if (best == "") {
      print "cycle" | "cat 1>&2"
      close("cat 1>&2")
      exit 3
    }
    EMITTED[best] = 1
    out = out best "\n"
    k = split(USERS[best], users, " ")
    for (i = 1; i <= k; i++) WAITING[users[i]]--
  }
  printf "%s", out
}' "$1"
SCRIPT
