cd /workspace || exit 1
awk '
index($0, "database.key_311 = ") == 1 { print "database.key_311 = 4096"; next }
index($0, "queue.key_555 = ") == 1 { next }
index($0, "cache.key_842 = ") == 1 { print "cache.key_842 = off"; next }
index($0, "auth.key_1204 = ") == 1 { print "auth.key_1204 = strict"; next }
{ print }
' settings.conf >settings.conf.new && mv settings.conf.new settings.conf
