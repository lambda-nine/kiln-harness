cd /workspace || exit 2
[ -f app.conf ] || exit 1
cmp -s - app.conf <<'END' || exit 1
# Application settings.
# Example: port=8080 serves plain HTTP.
name=kiln-demo
host=0.0.0.0
port=9090
admin_port=8080
  # Indented comments are kept too.
log_level=info
timeout_secs=30

max_connections=128
END
