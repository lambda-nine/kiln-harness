#!/bin/sh
set -eu

# The kiln this harness is tested against. It must match the kiln pin in
# README.md's "Pinned control plane" section.
PINNED_KILN=e48f0e785ad484d0b2947897f6d48d0756059d7e

# The Linux ACL tests skip without these tools; this run must not. Install
# nothing here: package changes on the shared VM go through its owner.
if ! command -v setfacl >/dev/null 2>&1 || ! command -v getfacl >/dev/null 2>&1; then
    echo "setfacl/getfacl not found: install Ubuntu's acl package (sudo apt-get install acl) so the Linux ACL tests run" >&2
    exit 1
fi

# kiln --version prints "kiln <version> (git <12-hex commit>, clean|dirty)",
# or "(git unknown)". Only a clean build of the pinned commit passes.
pinned_short=$(printf '%s' "$PINNED_KILN" | cut -c1-12)
kiln_version=$(sudo /usr/local/bin/kiln --version) || {
    echo "cannot run the installed kiln's --version" >&2
    exit 1
}
installed=$(printf '%s\n' "$kiln_version" |
    sed -n 's/^kiln [^ ]* (git \([0-9a-f]\{12\}\), \([a-z]*\))$/\1 \2/p')
installed_short=${installed%% *}
case "$installed" in
"") installed_short=unknown ;;
*" clean")
    case "$PINNED_KILN" in
    "$installed_short"*) installed_short= ;;
    esac
    ;;
*) installed_short="$installed_short (${installed#* })" ;;
esac
if test -n "$installed_short"; then
    echo "installed kiln $installed_short does not match the pinned $pinned_short; install the pinned kiln or update the pin" >&2
    echo "kiln --version: $kiln_version" >&2
    exit 1
fi

# Linux clippy and the host suite first, as the invoking user rather than
# root: the suite once with that user's umask and once with Ubuntu's
# user-private-group default, under which a careless test directory would be
# group-writable.
test "$(id -u)" != 0
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
(umask 0002 && cargo test --locked)

# The node may hold sandboxes this run does not own, such as a failed one
# kilnd keeps charged because its absence is unproven. Require no activity,
# then require the reservations and non-deleted sandbox IDs recorded before
# the run.
assert_clean() {
    health=$(sudo /usr/local/bin/kilndctl health)
    printf '%s\n' "$health"
    active=$(printf '%s\n' "$health" | awk '/^active sandboxes:/ { print $3 }')
    running=$(printf '%s\n' "$health" | awk '/^running executions:/ { print $3 }')
    test "$active" = "0"
    test "$running" = "0"
    reservations=$(printf '%s\n' "$health" | awk '/^node reservations:/ { print }')
    test -n "$reservations"
    listing=$(sudo /usr/local/bin/kilndctl ls)
    sandboxes=$(printf '%s\n' "$listing" | awk 'NR > 1 { print $1 }' | sort)
    snapshot=$(printf '%s\n%s' "$reservations" "$sandboxes")
    if test -n "${baseline+set}"; then
        test "$snapshot" = "$baseline"
    else
        baseline=$snapshot
    fi

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

# The CLI records, verifies, and reproduces one session.
bin=$(cargo build --locked --bin kiln-harness --message-format=json | \
    python3 -c '
import json, sys
matches = []
for line in sys.stdin:
    event = json.loads(line)
    if (event.get("reason") == "compiler-artifact"
            and event.get("target", {}).get("name") == "kiln-harness"
            and event.get("executable")):
        matches.append(event["executable"])
print(matches[-1] if matches else "")
')
test -n "$bin"
# Journals must live in a directory owned by the user running the harness.
work=$(sudo mktemp -d)
trap 'sudo rm -rf "$work"' EXIT
sudo "$bin" demo --audit "$work/demo-audit.jsonl" --journal "$work/demo.jsonl"
verify=$(sudo "$bin" journal verify "$work/demo.jsonl") || {
    printf '%s\n' "$verify"
    exit 1
}
printf '%s\n' "$verify"
printf '%s\n' "$verify" | grep -qx 'status: complete (completed)'
printf '%s\n' "$verify" | grep -qx 'sandbox deleted: yes'
replay=$(sudo "$bin" replay --from "$work/demo.jsonl" \
    --audit "$work/replay-audit.jsonl" --journal "$work/replay.jsonl") || {
    printf '%s\n' "$replay"
    exit 1
}
printf '%s\n' "$replay"
printf '%s\n' "$replay" | grep -qx reproduced
sudo "$bin" journal verify "$work/replay.jsonl"

assert_clean
