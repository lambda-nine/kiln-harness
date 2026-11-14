# ip.sh: IPv4 addresses.

# is_ipv4 TEXT: four numbers from 0 to 255 separated by dots.
is_ipv4() {
  printf '%s\n' "$1" | awk -F . '
    NF != 4 { exit 1 }
    { for (i = 1; i <= 4; i++) if ($i !~ /^[0-9]+$/ || $i > 255) exit 1 }
  '
}
