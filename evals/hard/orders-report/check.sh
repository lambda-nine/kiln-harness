cd /workspace || exit 2
LC_ALL=C
export LC_ALL
rm -rf /workspace/.check && mkdir /workspace/.check || exit 2
C=/workspace/.check
[ -f report.txt ] || exit 1
[ "$(find data docs | sort)" = "data
data/orders.csv
data/returns.csv
data/sku_aliases.txt
docs
docs/REPORT_RULES.md" ] || exit 1
[ "$(sha256sum data/orders.csv | cut -d ' ' -f 1)" = "0cc2f1f96fcc401f5a6869b7ead8f0f4d9d39c8310575f655c3d0095c8319f8b" ] || exit 1
[ "$(sha256sum data/returns.csv | cut -d ' ' -f 1)" = "f1891d23ed7ac70936d9380ee72925f299b317876708edebfa9a2e77366ceed1" ] || exit 1
[ "$(sha256sum data/sku_aliases.txt | cut -d ' ' -f 1)" = "b508fa3d383bd1ac7943da920130126b81c776a3a81f2516625566171f37b8d2" ] || exit 1
[ "$(sha256sum docs/REPORT_RULES.md | cut -d ' ' -f 1)" = "f79409e8dda05b480601d71923d6d20ed6597987b73124670e9d9dc362bcea24" ] || exit 1
tab=$(printf '\t')
sed "s/<TAB>/$tab/g" >"$C/want" <<'END'
sku<TAB>units<TAB>revenue
GZM-1<TAB>40<TAB>4800.00
LMP-9<TAB>39<TAB>1755.00
WGT-200<TAB>55<TAB>1347.50
WGT-100<TAB>45<TAB>899.55
CBL-HDMI<TAB>33<TAB>412.50
ZIP-3<TAB>10<TAB>412.50
CBL-USB<TAB>35<TAB>314.65
PIN-5<TAB>58<TAB>2.90
TOTAL<TAB>315<TAB>9944.60
END
cmp -s report.txt "$C/want" || exit 1
