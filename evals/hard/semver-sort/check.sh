cd /workspace || exit 2
LC_ALL=C
export LC_ALL
rm -rf /workspace/.check && mkdir /workspace/.check || exit 2
C=/workspace/.check
[ -f semver-sort.sh ] || exit 1
n=0
# run STATUS: sort $C/in; stdout must equal $C/want and the exit status
# STATUS; for status 2, stderr must equal $C/err.
run() {
  n=$((n + 1))
  sh /workspace/semver-sort.sh <"$C/in" >"$C/out" 2>"$C/got-err"
  rc=$?
  if [ "$rc" != "$1" ] || ! cmp -s "$C/out" "$C/want"; then
    echo "case $n"
    exit 1
  fi
  if [ "$1" = 2 ] && ! cmp -s "$C/got-err" "$C/err"; then
    echo "case $n"
    exit 1
  fi
}

# 1: the prompt's example.
cat >"$C/in" <<'END'
1.0.0
0.9.12
1.0.0-rc.1
0.10.0
1.0.0-beta.11
1.0.0-beta.2
0.9.2
2.0.0+build.7
1.0.0-alpha
1.0.0+20130313144700
1.0.0-alpha.1
2.0.0-rc.1
END
cat >"$C/want" <<'END'
0.9.2
0.9.12
0.10.0
1.0.0-alpha
1.0.0-alpha.1
1.0.0-beta.2
1.0.0-beta.11
1.0.0-rc.1
1.0.0
1.0.0+20130313144700
2.0.0-rc.1
2.0.0+build.7
END
run 0

# 2: the specification's chain, shuffled.
cat >"$C/in" <<'END'
1.0.0-beta.11
1.0.0
1.0.0-alpha.beta
1.0.0-rc.1
1.0.0-alpha
1.0.0-beta
1.0.0-beta.2
1.0.0-alpha.1
END
cat >"$C/want" <<'END'
1.0.0-alpha
1.0.0-alpha.1
1.0.0-alpha.beta
1.0.0-beta
1.0.0-beta.2
1.0.0-beta.11
1.0.0-rc.1
1.0.0
END
run 0

# 3: numeric, not textual, major, minor and patch.
cat >"$C/in" <<'END'
10.0.0
2.0.0
1.10.0
1.9.0
1.2.10
1.2.9
0.0.0
END
cat >"$C/want" <<'END'
0.0.0
1.2.9
1.2.10
1.9.0
1.10.0
2.0.0
10.0.0
END
run 0

# 4: numeric before alphanumeric, ASCII order, identifiers that look like
# numbers to awk, and a hyphen inside an identifier.
cat >"$C/in" <<'END'
1.0.0-a
1.0.0-10
1.0.0-1e5
1.0.0-9
1.0.0-Z
1.0.0-200
1.0.0-alpha-1
1.0.0-alpha.1
1.0.0-alpha
1.0.0-0a
1.0.0-0
END
cat >"$C/want" <<'END'
1.0.0-0
1.0.0-9
1.0.0-10
1.0.0-200
1.0.0-0a
1.0.0-1e5
1.0.0-Z
1.0.0-a
1.0.0-alpha
1.0.0-alpha.1
1.0.0-alpha-1
END
run 0

# 5: numbers too long for a floating-point compare.
cat >"$C/in" <<'END'
123456789012345678902.0.0
123456789012345678901.0.0
99999999999999999999.0.0
1.0.0-123456789012345678901234567890
1.0.0-123456789012345678901234567889
1.1234567890123456789012346.0
1.1234567890123456789012345.0
END
cat >"$C/want" <<'END'
1.0.0-123456789012345678901234567889
1.0.0-123456789012345678901234567890
1.1234567890123456789012345.0
1.1234567890123456789012346.0
99999999999999999999.0.0
123456789012345678901.0.0
123456789012345678902.0.0
END
run 0

# 6: build metadata is ignored, and equal versions keep their order.
cat >"$C/in" <<'END'
1.0.0+zeta
1.0.0-rc.1+b
2.0.0
1.0.0+alpha
1.0.0-rc.1+a
1.0.0
0.9.0+x.007
1.0.0+001
END
cat >"$C/want" <<'END'
0.9.0+x.007
1.0.0-rc.1+b
1.0.0-rc.1+a
1.0.0+zeta
1.0.0+alpha
1.0.0
1.0.0+001
2.0.0
END
run 0

# 7: no LF after the last line.
printf '1.0.1\n1.0.0-rc.1+build.5\n1.0.0' >"$C/in"
printf '1.0.0-rc.1+build.5\n1.0.0\n1.0.1\n' >"$C/want"
run 0

# 8: empty input.
: >"$C/in"
: >"$C/want"
run 0

# 9 onwards: each invalid form, at different lines.
: >"$C/want"
i=0
for bad in 01.2.3 1.2 1.2.3- 1.2.3-alpha..1 1.2.3-01 v1.2.3 1.2.3+ \
  1.2.3.4 1.2.3-beta_1 1.0.0+build..1 1.2.3-.a; do
  i=$((i + 1))
  case $((i % 3)) in
    0) printf '%s\n1.0.0\n' "$bad" >"$C/in"; line=1 ;;
    1) printf '1.0.0\n0.1.0-rc.1\n%s\n2.0.0\n' "$bad" >"$C/in"; line=3 ;;
    2) printf '1.0.0\n2.0.0-x\n3.0.0\n4.0.0\n%s' "$bad" >"$C/in"; line=5 ;;
  esac
  printf 'invalid version on line %d: %s\n' "$line" "$bad" >"$C/err"
  run 2
done

# A space, an empty line, and only the first invalid line reported.
printf '1.0.0\n1.0.0 \n' >"$C/in"
printf 'invalid version on line 2: 1.0.0 \n' >"$C/err"
run 2
printf '1.0.0\n\n2.0.0\n' >"$C/in"
printf 'invalid version on line 2: \n' >"$C/err"
run 2
printf '1.0.0\n2.0.0\n1.2\n3.0.0\n01.0.0\n' >"$C/in"
printf 'invalid version on line 3: 1.2\n' >"$C/err"
run 2
# Input that is one empty line is one empty version, not empty input.
printf '\n' >"$C/in"
printf 'invalid version on line 1: \n' >"$C/err"
run 2
