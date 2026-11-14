cd /workspace || exit 2
LC_ALL=C
export LC_ALL
# Regular files and directories only: no symlinks or other special entries.
[ -z "$(find . ! -type f ! -type d)" ] || { echo "not a regular file"; exit 1; }
# Every file of the expected tree, by SHA-256: nothing added, removed, left
# behind or changed outside the request.
want=$(mktemp) || exit 2
got=$(mktemp) || { rm -f "$want"; exit 2; }
cat >"$want" <<'END'
8e7f9fa3fc181f3b5c11ded810b600b27fbcad11defd25b8488c838cb212ffac  ./CHANGELOG.md
642781d4c38d4d0fa666a1cd9425ae3e1a1e7b188bb533cf19a87e5a92772951  ./README.md
2aca27d0487105612d5b4643e108ca994fb9827cca0f2057f092408ab5e334f3  ./bin/compare.sh
adc3c8dbc5270ae049a0109160d9a53e5cc601c8a24d3255fe1e06bdee2bab3f  ./bin/dus.sh
3b4c3d29c68883d11bbb2ac924a6809795a76500049ab2da095c4eb22e742f61  ./bin/quota.sh
bb9b7e7f38cca21b207d615841611778d53e05765d833fefe58f22ba118243ad  ./bin/sizeof.sh
09412248b97d04159bfc9e7c6377bb940be879b91666f57ee0f2010931de3fee  ./bin/topn.sh
ad16f2e169655266cdd86b2a099f42c00d857ea0129475a05a0626c71bbc3a26  ./contrib/du2scan.sh
de8631953e62d52b3a0fe267a94a15a92b0aefe141e2276150a808027125a815  ./contrib/explain.sh
757ee17e8521a1f0c49d08e1e91b80bb3ddf367f58bf4fcf11e7fa0530653b57  ./docs/usage.md
d6b2cac7f68d1e531107d3a1325b6071d7d034dbe076960d514a4d10bd3e6eba  ./etc/dus.conf
f90185bd409d858c0b7055d0a47836f44badcf446fd49303f2e3c8681bf5f3f4  ./lib/awk/hist.sh
982450ec7bb43e3278abaca4e6de7f9ac580ff2563aea61a3aaf37c435a25d33  ./lib/color.sh
fc34ed7f1d2f0b50b066a8754d2dd251a411118e4198eded0c9e731cbde14be5  ./lib/fmt/bar.sh
8e821cecf0e66c20ac153cc6ab500da7c3ed927d08c209bebe46175c3448edba  ./lib/fmt/json.sh
a6a4719b7dfe343b75b024f0ba82e14055aa32fae6b59782dd6819dbb501502b  ./lib/fmt/legacy.sh
242f823240296bd2d79f5a2b4726864fd778f0c5f3d87f07c2ba3b0ef625f39f  ./lib/fmt/pad.sh
8df2abddef477f93130f0debf3ac149968ac4b4058ab0e7f38fad5f070aba945  ./lib/fmt/table.sh
5ee459ed4ec82ad79cc13453432879c43f6fec1733717d5adc838ed118f4955a  ./lib/log.sh
58bb4057a163b73fa1339d86dca57220b9f0f25ecbf652a0203fe6418b845a46  ./lib/parse.sh
9ae1bca8ede0b3cb936da1f1748b2a7df015d54e305fdd7b83f0c1acab76a323  ./lib/paths.sh
1e8c89b106a635b70c78b1193f1a9aeb5672d001ab7620209a54ab56ba3910d5  ./lib/quota.sh
633d8e663eecf6076742fe50a5f505860e65a1f3a402f4ff58c91990ae71d2b6  ./lib/report/csv.sh
71bd374f539db63f86971e071ac7583e9283374f135f0dcf11ec1167f569948d  ./lib/report/diff.sh
ac7858ebe77ae7a098bd34d506ba1ebea87c3f2e1115c0bc87bcd2f58d7a857e  ./lib/report/summary.sh
3458d74080933d93b971a13d18b0c4bf99b962ee32140398abf526ab8e702a3d  ./lib/report/top.sh
2e120a7b93dea91a6802e4170bca5c912fbab8c7f51e79c1083103c16a01b238  ./lib/scan.sh
50281d21813fbf7abaed673538b56d0349346cbafdd8ebeab14660b4f26b9344  ./lib/size.sh
c7e27a3621111351351718cf033ea1a5c3313462167d7160e8a317a501a1fa52  ./lib/units.sh
1c2ebde6ab9080f29159c7c138a6fa951c786e12e626e49602d3722f19d8dc77  ./lib/util.sh
e8bfb71ff19423a39858d124a2ccae11cd32a10124196d386027aaae01884351  ./share/hist.awk
8d81d51c956e4189ff6cb21569358597c6fda203210fd60a875708ee203fb846  ./tests/fixtures/old-scan.txt
35da6012824268c31253d0fda5a2c4847cc2231b0e8d3908a3dc72beb8aa5b22  ./tests/fixtures/scan.txt
f47941e8325fdc07c6cd35402e1c9f8eedeb47f11ec3e9809e3185ec188274c4  ./tests/fixtures/sizes.txt
0ff35ca637aa95364a7a01f2b1b7968872a0931bf717be3452920651e6ba3795  ./tests/fixtures/summary.txt
52cadcaf11ecc07552c35b3c2024347e14f415ce822d45d213e51b81ff4c5f88  ./tests/run.sh
END
find . -type f | sort | while IFS= read -r f; do
  printf '%s  %s\n' "$(sha256sum "$f" | cut -d ' ' -f 1)" "$f"
done >"$got"
if ! cmp -s "$want" "$got"; then
  bad=$(awk 'NR == FNR { h[$2] = $1; next } h[$2] != $1 { print $2; exit }' "$want" "$got")
  rm -f "$want" "$got"
  echo "${bad:-files}"
  exit 1
fi
rm -f "$want" "$got"
# The project's own tests.
sh tests/run.sh </dev/null >/dev/null 2>&1 || { echo "tests"; exit 1; }
exit 0
