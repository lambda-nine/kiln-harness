cd /workspace || exit 2
LC_ALL=C
export LC_ALL
# Regular files and directories only: no symlinks or other special entries.
[ -z "$(find . ! -type f ! -type d)" ] || { echo "not a regular file"; exit 1; }
# Every file but lib/log.sh by SHA-256, and lib/log.sh by name: nothing
# added, removed, left behind or changed outside the request.
want=$(mktemp) || exit 2
got=$(mktemp) || { rm -f "$want"; exit 2; }
cat >"$want" <<'END'
27f6a5ed246865b562d8b6c9e07758fcdade6727bd9145946dd8d5173c76bbc1  ./CHANGELOG.md
7bb1d7f11e23d5b8a987a423fcfc839b290f86f9060bc8957a5b961b8a1b43c2  ./README.md
5923e0dc64c285a46a826724b7d313af3c3e5f4b0086b91e9f4f10ff724f9429  ./bin/backup.sh
7a7e18e47e415626e6ee3e15209b428251a9730b3be111c11153f3f72e8f6170  ./bin/nightly
1481bae60c8a36ef9bb38c376290cd78e0b02af0f94273b962da66a101abac28  ./bin/rotate.sh
1f633b304b16836db22bdd51ad30869accbcb56eda04e227b22f42ede69cb390  ./bin/summary.sh
ceb8866b6d7ca91439c66972b845ad80d6be1cf74736b069729332202237a463  ./bin/sync-mirror.sh
b0cab4703b1066b354c6661e6f7914694b262a2b83713e7f193baf54c1191bfa  ./etc/nightly.conf
bd874f24fa2d4665ddc496ff77429db558753daed00d7a47c275b37207c398c0  ./lib/config.sh
3cdc8326f05cecb4e3de9b5caf6d3241722685a93858f690d9a6d12951c02204  ./lib/db.sh
5ed1b3f33110a6e8549c3663cc77a278c031674ef76f16fb0a24cdf6ad5b81a8  ./lib/fs/prune.sh
3bc9c70d91e632d0ab07d368fadca3fdeb5248023b6654c0c165cb53270ce41f  ./lib/lock.sh
-  ./lib/log.sh
e83368efce7c0857c630163cf6645ac1df3158cd151e469035dd0fbb8d500c39  ./lib/mail.sh
508dea4fb141660c90fb9276adc81898adaa59d66a556a7e2f5f75bab488ec87  ./lib/net/fetch.sh
344ac854d97b255d5f9e54f32695294d98b0c06f941428cf51680b837de624e9  ./lib/report.sh
704f03ab633768380344037be813cea4c568f79d9bcba2de58fee131b3f1b7b5  ./tests/run.sh
END
find . -type f | sort | while IFS= read -r f; do
  if [ "$f" = ./lib/log.sh ]; then
    h=-
  else
    h=$(sha256sum "$f" | cut -d ' ' -f 1)
  fi
  printf '%s  %s\n' "$h" "$f"
done >"$got"
if ! cmp -s "$want" "$got"; then
  bad=$(awk 'NR == FNR { h[$2] = $1; next } h[$2] != $1 { print $2; exit }' "$want" "$got")
  rm -f "$want" "$got"
  echo "${bad:-files}"
  exit 1
fi
rm -f "$want" "$got"
# lib/log.sh still ends with a newline and holds no NUL bytes, so the
# line-based comparisons below see every byte of it.
[ "$(tail -c 1 lib/log.sh | od -An -tx1 | tr -d ' \n')" = 0a ] || { echo "log.sh final newline"; exit 1; }
[ "$(tr -d '\000' <lib/log.sh | wc -c)" = "$(wc -c <lib/log.sh)" ] || { echo "log.sh NUL bytes"; exit 1; }
# lib/log.sh: unchanged before log_event's comment and from log_tail's
# comment on.
[ "$(head -n 17 lib/log.sh | sha256sum | cut -d ' ' -f 1)" = 4e19ea86125ef486a6ca68c69904f5e44354c52fcabcfc84584ebee83680ee03 ] || { echo "log.sh head"; exit 1; }
[ "$(awk 'f || /^# log_tail / { f = 1; print }' lib/log.sh | sha256sum | cut -d ' ' -f 1)" = 690c8e8b1d048c386f11abd5341cf6d6d570c967ce8288bf6fdb63919d501130 ] || { echo "log.sh tail"; exit 1; }
# log_event itself, with and without a component.
tmp=$(mktemp -d) || exit 2
(
  unset LOG_LEVEL LOG_FILE
  . ./lib/log.sh
  [ "$(log_event info hello 2>&1)" = "info: hello" ] || exit 1
  [ "$(log_event warn "two  words" net 2>&1)" = "warn [net]: two  words" ] || exit 1
  [ "$(log_event error x '' 2>&1)" = "error: x" ] || exit 1
  [ -z "$(log_event info m c 2>/dev/null)" ] || exit 1
  [ -z "$(LOG_LEVEL=warn log_event info m c 2>&1)" ] || exit 1
  [ "$(LOG_LEVEL=warn log_event error m c 2>&1)" = "error [c]: m" ] || exit 1
  [ "$(LOG_LEVEL=debug log_event debug d sync-mirror 2>&1)" = "debug [sync-mirror]: d" ] || exit 1
  [ "$(LOG_LEVEL=error log_event notice n a 2>&1)" = "notice [a]: n" ] || exit 1
  [ "$(log_event info '50% \n "q" $x' x 2>&1)" = 'info [x]: 50% \n "q" $x' ] || exit 1
  [ "$(log_event warn "$(printf 'a\nb')" y 2>&1)" = "$(printf 'warn [y]: a\nb')" ] || exit 1
  LOG_FILE=$tmp/log
  log_event warn w c 2>/dev/null
  log_event debug hidden c 2>/dev/null
  log_event info i 2>/dev/null
  [ "$(cat "$tmp/log")" = "$(printf 'warn [c]: w\ninfo: i')" ] || exit 1
) </dev/null >/dev/null 2>&1 || { rm -rf "$tmp"; echo "log_event"; exit 1; }
rm -rf "$tmp"
# The project's own tests.
sh tests/run.sh </dev/null >/dev/null 2>&1 || { echo "tests"; exit 1; }
exit 0
