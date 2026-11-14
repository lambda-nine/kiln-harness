cd /workspace || exit 1
# A year divisible by 400 is a leap year too.
awk '
$0 == "  [ $(( $1 % 4 )) -eq 0 ] && [ $(( $1 % 100 )) -ne 0 ]" {
  print "  { [ $(( $1 % 4 )) -eq 0 ] && [ $(( $1 % 100 )) -ne 0 ]; } || [ $(( $1 % 400 )) -eq 0 ]"
  next
}
{ print }
' lib/calendar.sh >lib/calendar.sh.new && mv lib/calendar.sh.new lib/calendar.sh
