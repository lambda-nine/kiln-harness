#!/bin/sh
set -eu

assert_clean() {
    health=$(sudo /usr/local/bin/kilndctl health)
    printf '%s\n' "$health"
    active=$(printf '%s\n' "$health" | awk '/^active sandboxes:/ { print $3 }')
    running=$(printf '%s\n' "$health" | awk '/^running executions:/ { print $3 }')
    reservations=$(printf '%s\n' "$health" | awk '/^node reservations:/ { print }')
    test "$active" = "0"
    test "$running" = "0"
    printf '%s\n' "$reservations" | grep -q 'sandboxes 0/'
    printf '%s\n' "$reservations" | grep -q 'memory 0/'
    printf '%s\n' "$reservations" | grep -q 'pids 0/'
    printf '%s\n' "$reservations" | grep -q 'disk 0/'

    registry_lines=$(sudo /usr/local/bin/kiln ls | wc -l | tr -d ' ')
    test "$registry_lines" = "1"
    test -z "$(sudo find /var/lib/kiln/runs -mindepth 1 -maxdepth 1 -print)"
    if test -d /sys/fs/cgroup/kiln; then
        test -z "$(sudo find /sys/fs/cgroup/kiln -mindepth 1 -type d -print)"
    fi
    test -z "$(sudo ps -eo comm=,args= | grep -E 'kiln-(shim|init)|kiln (shim|init)' | grep -v grep || true)"
}

assert_clean

test_bin=$(cargo test --locked --test real_kilnd --no-run --message-format=json | \
    python3 -c '
import json, sys
matches = []
for line in sys.stdin:
    event = json.loads(line)
    if (event.get("reason") == "compiler-artifact"
            and event.get("target", {}).get("name") == "real_kilnd"
            and event.get("executable")):
        matches.append(event["executable"])
print(matches[-1] if matches else "")
')
test -n "$test_bin"
sudo env KILND_INTEGRATION=1 "$test_bin" \
    --exact scripted_provider_through_installed_kilnd_leaves_no_api_residue \
    --nocapture

assert_clean
