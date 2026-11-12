cd /workspace || exit 2
LC_ALL=C
export LC_ALL
rm -rf /workspace/.check && mkdir /workspace/.check || exit 2
C=/workspace/.check
[ -f lib/strutil.sh ] || exit 1
[ "$(sha256sum docs/strutil.md | cut -d ' ' -f 1)" = "46f0d997dd1a94c8aba9e29cbfc92d220a0c7638e1105abb91f40a6dc664ccbc" ] || exit 1
[ "$(sha256sum tests/test_strutil.sh | cut -d ' ' -f 1)" = "199e115ea72ee63349b69e3a566662a7e7c417001392b9b554a8d008f546cf47" ] || exit 1
[ "$(find docs tests | sort)" = "docs
docs/strutil.md
tests
tests/test_strutil.sh" ] || exit 1
sh tests/test_strutil.sh </dev/null >/dev/null 2>&1 || { echo "visible tests"; exit 1; }

# Every documented rule, in a subshell so the library cannot affect the check.
(
. ./lib/strutil.sh
tab=$(printf '\t')
cr=$(printf '\r')
i=keep n=keep out=keep result=keep s=keep count=keep rest=keep c=keep ch=keep pad=keep w=keep width=keep
t() {
  "$@"
  echo "rc=$?"
}
t str_trim '  a b  '
t str_trim "$tab x${tab}y $tab"
t str_trim " $tab "
t str_trim ''
t str_trim " ${cr}a$cr "
t str_trim '  -n '
t str_trim 'plain'
t str_pad_left 7 3 0
t str_pad_left abc 3
t str_pad_left abcd 2
t str_pad_left '' 2
t str_pad_left x 0
t str_pad_left 'a b' 5
t str_pad_left x 4 '*'
t str_pad_left x -1
t str_pad_left x ''
t str_pad_left x 3a
t str_pad_left x 4 ''
t str_pad_left x 4 ab
t str_repeat ab 0
t str_repeat ab 1
t str_repeat 'x ' 3
t str_repeat '' 5
t str_repeat '*' 4
t str_repeat ab -1
t str_repeat ab ''
t str_repeat ab 2x
t str_pad_left x 010 0
t str_pad_left x 08
t str_repeat ab 010
t str_repeat ab 08
t str_repeat ab 00
t str_count aaaa aa
t str_count aaaaa aa
t str_count abababa aba
t str_count abcabc abc
t str_count abc d
t str_count '' a
t str_count abc ''
t str_count 'a*b a*b axb' 'a*b'
t str_count 'x?y xzy' '?'
t str_count 'a\b a\\b' '\'
t str_count '[ab] a b' '[ab]'
t str_lower 'Hello, WORLD-123 ÄÉ é'
t str_lower '[@Z`a{]'
t str_lower '-n'
t str_lower ''
t str_starts_with foobar foo
t str_starts_with foobar ''
t str_starts_with '' ''
t str_starts_with foo foobar
t str_starts_with abc 'a*'
t str_starts_with 'a*c' 'a*'
t str_starts_with abc '?'
t str_starts_with abc '[a]'
t str_starts_with '[a]bc' '[a]'
echo "vars $i $n $out $result $s $count $rest $c $ch $pad $w $width"
) </dev/null >"$C/out" 2>/dev/null

tab=$(printf '\t')
cr=$(printf '\r')
sed -e "s/<TAB>/$tab/g" -e "s/<CR>/$cr/g" -e "s/<SP>/ /g" >"$C/want" <<'END'
a b
rc=0
x<TAB>y
rc=0

rc=0

rc=0
<CR>a<CR>
rc=0
-n
rc=0
plain
rc=0
007
rc=0
abc
rc=0
abcd
rc=0
<SP><SP>
rc=0
x
rc=0
  a b
rc=0
***x
rc=0
rc=2
rc=2
rc=2
rc=2
rc=2

rc=0
ab
rc=0
x x x<SP>
rc=0

rc=0
****
rc=0
rc=2
rc=2
rc=2
000000000x
rc=0
       x
rc=0
abababababababababab
rc=0
abababababababab
rc=0

rc=0
2
rc=0
2
rc=0
2
rc=0
2
rc=0
0
rc=0
0
rc=0
rc=2
2
rc=0
1
rc=0
3
rc=0
1
rc=0
hello, world-123 ÄÉ é
rc=0
[@z`a{]
rc=0
-n
rc=0

rc=0
rc=0
rc=0
rc=0
rc=1
rc=1
rc=0
rc=1
rc=1
rc=0
vars keep keep keep keep keep keep keep keep keep keep keep keep
END
cmp -s "$C/out" "$C/want" || { echo "hidden tests"; exit 1; }
