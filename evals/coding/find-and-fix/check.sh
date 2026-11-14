cd /workspace || exit 2
LC_ALL=C
export LC_ALL
# The same files as the seed: nothing added, removed or left behind.
want=$(mktemp) || exit 2
cat >"$want" <<'END'
./README.md
./bin/daysin.sh
./bin/fmtbytes.sh
./bin/monthcal.sh
./bin/quarter.sh
./bin/textkit.sh
./bin/wordcount.sh
./lib/calendar.sh
./lib/colors.sh
./lib/config.sh
./lib/core.sh
./lib/csv.sh
./lib/dates.sh
./lib/fiscal.sh
./lib/fmt/bytes.sh
./lib/fmt/duration.sh
./lib/fmt/number.sh
./lib/fmt/table.sh
./lib/hash.sh
./lib/io/lines.sh
./lib/lists.sh
./lib/log.sh
./lib/net/ip.sh
./lib/net/url.sh
./lib/numbers.sh
./lib/paths.sh
./lib/seasons.sh
./lib/strings.sh
./lib/text/case.sh
./lib/text/count.sh
./lib/text/wrap.sh
./lib/time/clock.sh
./lib/time/week.sh
./lib/units.sh
./lib/validate.sh
./tests/run.sh
END
find . -type f | sort | cmp -s - "$want" || { rm -f "$want"; echo "files"; exit 1; }
rm -f "$want"
# The tests are unchanged.
[ "$(sha256sum tests/run.sh | cut -d ' ' -f 1)" = fc93c6e22ae7360554cfed1e5378ddc8d5c87ec4db0c48e10f430a5c582e1e37 ] || { echo "tests changed"; exit 1; }
sh tests/run.sh </dev/null >/dev/null 2>&1 || { echo "tests"; exit 1; }
# The leap year rule itself, on years the tests do not try.
(
  . ./lib/core.sh
  . ./lib/calendar.sh
  . ./lib/dates.sh
  for pair in 1600:0 1700:1 1800:1 1996:0 2000:0 2004:0 2100:1 2400:0 2023:1 4:0 100:1 400:0; do
    year=${pair%:*}
    if is_leap_year "$year"; then got=0; else got=1; fi
    [ "$got" = "${pair#*:}" ] || exit 1
  done
  [ "$(days_in_month 2 2400)" = 29 ] || exit 1
  [ "$(days_in_month 2 2100)" = 28 ] || exit 1
  [ "$(days_in_month 12 2000)" = 31 ] || exit 1
  [ "$(day_of_year 31 12 2000)" = 366 ] || exit 1
  [ "$(day_of_year 31 12 1900)" = 365 ] || exit 1
  valid_date 29 2 1600 || exit 1
  ! valid_date 29 2 1700 || exit 1
) </dev/null >/dev/null 2>&1 || { echo "calendar"; exit 1; }
# The rest of calendar.sh, the module the fix is in, still works.
(
  . ./lib/calendar.sh
  m=1
  for name in January February March April May June July August September October November December; do
    [ "$(month_name "$m")" = "$name" ] || exit 1
    [ "$(month_abbr "$m")" = "$(printf '%s\n' "$name" | cut -c 1-3)" ] || exit 1
    m=$((m + 1))
  done
  ! month_name 13 >/dev/null || exit 1
  [ -z "$(month_name 0)" ] || exit 1
  ! month_abbr 13 >/dev/null || exit 1
  [ -z "$(month_abbr 0)" ] || exit 1
) </dev/null >/dev/null 2>&1 || { echo "months"; exit 1; }
# And so do the other modules, on inputs the tests do not try.
(
  for module in core strings numbers lists paths seasons fiscal csv units validate colors \
    time/week time/clock fmt/number fmt/duration fmt/table fmt/bytes text/case text/count \
    net/url net/ip; do
    . "./lib/$module.sh" || exit 1
  done
  [ "$(trim '  x y ')" = "x y" ] && [ "$(to_lower ABC)" = abc ] || exit 1
  starts_with abcdef abc && ! ends_with abcdef abc && ends_with abcdef def || exit 1
  [ "$(repeat_str xy 0)" = "" ] && [ "$(repeat_str - 4)" = ---- ] || exit 1
  [ "$(clamp 5 1 9)" = 5 ] && [ "$(max_of 3 9 2)" = 9 ] && [ "$(min_of 3 9 2)" = 2 ] || exit 1
  is_even 4 && ! is_even 7 && [ "$(sum_list 5 -2)" = 3 ] || exit 1
  [ "$(join_by ', ' x y)" = "x, y" ] && [ "$(count_items a b c)" = 3 ] || exit 1
  [ "$(first_item a b c)" = a ] && [ "$(last_item a b c)" = c ] || exit 1
  contains_item b a b c && ! contains_item d a b c || exit 1
  [ "$(path_base /a/b/c.txt)" = c.txt ] && [ "$(path_dir /a/b/c.txt)" = /a/b ] || exit 1
  [ "$(path_dir c.txt)" = . ] && [ "$(path_ext /a.d/README)" = "" ] || exit 1
  [ "$(season_of 2)" = winter ] && [ "$(season_of 7)" = summer ] && ! season_of 13 || exit 1
  [ "$(fiscal_quarter 2)" = 4 ] && [ "$(fiscal_year 2 2024)" = 2023 ] || exit 1
  [ "$(csv_field a,b,c 2)" = b ] && [ "$(csv_count a,b,c)" = 3 ] || exit 1
  [ "$(c_to_f 100)" = 212 ] && [ "$(f_to_c 212)" = 100 ] && [ "$(km_to_m 3)" = 3000 ] || exit 1
  is_hex 0aF && ! is_hex 0g && is_email a@b.c && ! is_email a@@b.c || exit 1
  is_int -12 && ! is_int 1-2 && ! is_int '' || exit 1
  NO_COLOR=1
  [ "$(green ok)" = ok ] || exit 1
  [ "$(weekday_name 3)" = Wednesday ] && is_weekend 7 && ! is_weekend 5 || exit 1
  [ "$(hms 3723)" = 1:02:03 ] && [ "$(to_seconds 1 2 3)" = 3723 ] || exit 1
  [ "$(with_commas -1234)" = -1,234 ] && [ "$(with_commas 999)" = 999 ] || exit 1
  [ "$(human_duration 59)" = 59s ] && [ "$(human_duration 61)" = "1m 1s" ] || exit 1
  [ "$(pad_left ab 4)" = "  ab" ] && [ "$(pad_right ab 4)x" = "ab  x" ] || exit 1
  [ "$(human_bytes 1536)" = "1.5 KiB" ] && [ "$(human_bytes 12)" = "12 B" ] || exit 1
  [ "$(title_case 'hello big world')" = "Hello Big World" ] || exit 1
  [ "$(snake_to_kebab a_b_c)" = a-b-c ] || exit 1
  [ "$(printf 'a b\nc\n' | count_words)" = 3 ] && [ "$(printf 'a\nb\n' | count_lines)" = 2 ] || exit 1
  [ "$(url_host https://example.com:8080/x)" = example.com ] || exit 1
  [ "$(url_scheme https://example.com)" = https ] && ! url_scheme example.com || exit 1
  is_ipv4 10.0.0.255 && ! is_ipv4 10.0.0.256 && ! is_ipv4 1.2.3 || exit 1
) </dev/null >/dev/null 2>&1 || { echo "modules"; exit 1; }
[ "$(sh bin/daysin.sh 2 2000 </dev/null 2>/dev/null)" = 29 ] || { echo "daysin"; exit 1; }
[ "$(sh bin/quarter.sh 2 2024 </dev/null 2>/dev/null)" = "Q4 FY2023" ] || { echo "quarter"; exit 1; }
[ "$(sh bin/fmtbytes.sh 1048576 </dev/null 2>/dev/null)" = "1.0 MiB" ] || { echo "fmtbytes"; exit 1; }
[ "$(sh bin/monthcal.sh 2400 </dev/null 2>/dev/null | sed -n 2p)" = "February  29" ] || { echo "monthcal"; exit 1; }
exit 0
