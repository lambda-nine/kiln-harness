cd /workspace || exit 2
LC_ALL=C
export LC_ALL
rm -rf /workspace/.check && mkdir /workspace/.check || exit 2
C=/workspace/.check
# Exactly these files, no others.
find . -type f | grep -v '^\./\.check/' | sort >"$C/files" || exit 2
cat >"$C/want" <<'END'
./CHANGELOG.md
./bin/app.sh
./docs/api.md
./lib/config.sh
./lib/log.sh
./lib/net.sh
./lib/sub/banner.sh
./lib/util.sh
./tests/run.sh
END
cmp -s "$C/files" "$C/want" || { echo "files"; exit 1; }
# Each with exactly the renamed content (CHANGELOG.md unchanged).
while read -r sum path; do
  [ "$(sha256sum "$path" | cut -d ' ' -f 1)" = "$sum" ] || { echo "$path"; exit 1; }
done <<'END'
77e4afd3d93987209cbfd128bab6ab2675d6092c393589bebbd0da097fa8b4fe  ./CHANGELOG.md
13a4cd12c6560f2980da07c573c942f9fb14ef7d920aaf7ccc72c1259656a752  ./bin/app.sh
612ebdf2e610483a16d6770bec949ad19747d30979a798ce3e175231cfb4e07f  ./docs/api.md
0f5952e676bc4d075606e4f5faf423be714413270d039416e96d5c630ea762f5  ./lib/config.sh
57e93f7ce13fa6bee28e90bdef610ee3fbff221ac730dd4b69f99401796e23d9  ./lib/log.sh
48af03bf48f7126844eb19e9db1b9e50894d73df1dadef142645b693df32b05b  ./lib/net.sh
cd103b34d5c108ddb48f0480bc21bb3c00966d7e13202bc4ceef2bc7ac7bad1f  ./lib/sub/banner.sh
879fa167d876f57dbc3aeabb590066245cf11606603573c43b89da5a9b13ee3a  ./lib/util.sh
d8cb2959be9a09390fa6279a0c468c76ad017c80332a70613ab616d841f6981b  ./tests/run.sh
END
sh tests/run.sh </dev/null >/dev/null 2>&1 || { echo "tests"; exit 1; }
(
  . ./lib/config.sh
  . ./lib/log.sh
  . ./lib/net.sh
  . ./lib/util.sh
  . ./lib/sub/banner.sh
  [ "$(config_get port)" = 8080 ] || exit 1
  [ "$(config_get name)" = demo ] || exit 1
  [ "$(get_cfg_path)" = etc/app.conf ] || exit 1
  [ "$(get_cfgs | wc -l | tr -d ' ')" = 4 ] || exit 1
  [ "$(my_get_cfg host)" = localhost ] || exit 1
  [ "$(require retries)" = 3 ] || exit 1
  ! command -v get_cfg >/dev/null 2>&1 || exit 1
  [ "$(sh bin/app.sh | tail -n 1)" = demo ] || exit 1
) </dev/null >/dev/null 2>&1 || { echo "behaviour"; exit 1; }
