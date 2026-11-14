cd /workspace || exit 1
# Track the section and whether the line continues a value; apply each change
# only inside its own section.
awk '
function key(line) {
  if (line ~ /^[ \t#;]/ || line !~ /=/) return ""
  sub(/[ \t]*=.*/, "", line)
  return line
}
function setval(line, value,   head) {
  head = line
  sub(/=.*/, "", head)
  rest = substr(line, length(head) + 2)
  match(rest, /^ */)
  return head "=" substr(rest, 1, RLENGTH) value
}
{
  cont = ($0 ~ /^[ \t]/)
  if ($0 ~ /^\[/) { sec = $0; skip = 0; inval = "" }
  else if (!cont) inval = key($0)
  # The setting after the replica hosts list: once hosts has ended.
  if (sec == "[prod.db.replica]" && after_hosts && !cont) {
    print "max_lag_seconds = 30"
    after_hosts = 0
  }
  if (skip && cont) next
  skip = 0
  if ($0 == "[staging.web]") { print "[staging.frontend]"; next }
  if (sec == "[prod.db]" && !cont && key($0) == "pool_size") { print setval($0, "40"); next }
  if (sec == "[canary.api]" && !cont && key($0) == "timeout") { print setval($0, "45"); next }
  if (sec == "[dev.cache]" && !cont && key($0) == "servers") { skip = 1; next }
  if (sec == "[prod.db.replica]" && !cont && key($0) == "hosts") after_hosts = 1
  if (sec == "[prod.web]" && inval == "allowed_origins") {
    if (cont && $0 ~ /^[ \t]+https:\/\/admin\.example\.com$/) next
    if (cont) { indent = $0; sub(/[^ \t].*/, "", indent) }
    origins = 1
  } else if (origins) {
    print indent "https://shop.example.com"
    origins = 0
  }
  if (sec == "[dr.scheduler]" && inval == "jobs" && cont && $0 ~ /^[ \t]+30 3 \* \* \* vacuum$/) next
  print
}
' app.ini >app.ini.new && mv app.ini.new app.ini
