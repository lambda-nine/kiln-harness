cd /workspace || exit 2
LC_ALL=C
export LC_ALL

n=24
precondition() {
    printf 'cases 0 of %s failed all\n' "$n"
    exit 1
}
broken() {
    echo 'probe-plan check could not prepare or run a case' >&2
    exit 2
}

command -v cmp >/dev/null 2>&1 || broken
[ -f plan.sh ] || precondition
[ -d docs ] && [ ! -L docs ] && [ -d examples ] && [ ! -L examples ] || precondition
paths=$(find docs examples) || broken
paths=$(printf '%s\n' "$paths" | sort) || broken
[ "$paths" = 'docs
docs/probe.md
examples
examples/adaptive.out
examples/adaptive.table
examples/impossible.out
examples/impossible.table
examples/priming.out
examples/priming.table
examples/tie.out
examples/tie.table' ] || precondition
for file in docs/probe.md examples/adaptive.out examples/adaptive.table examples/impossible.out examples/impossible.table examples/priming.out examples/priming.table examples/tie.out examples/tie.table; do
    [ -f "$file" ] && [ ! -L "$file" ] || precondition
done
hashes=$(
    for file in docs/probe.md examples/adaptive.out examples/adaptive.table examples/impossible.out examples/impossible.table examples/priming.out examples/priming.table examples/tie.out examples/tie.table; do
        digest=$(sha256sum "$file") || exit 2
        printf '%s\n' "${digest%% *}"
    done
) || broken
hash=$(printf '%s\n' "$hashes" | sha256sum) || broken
[ "${hash%% *}" = 'fa76c1142b6c8eecc01cb61d269dca881ca20fd867b02c4d735de5cc13c4b2d3' ] || precondition

umask 077
scratch=$(mktemp -d /workspace/.probe-check.XXXXXX) || broken
solver=
group=
watchdog=
use_setsid=0
if command -v setsid >/dev/null 2>&1; then
    use_setsid=1
fi

kill_solver() {
    if [ "$use_setsid" -eq 1 ]; then
        # BusyBox kill recognizes the negative group ID after -9.
        if [ -f "$scratch/$number.started" ]; then
            kill -9 "-$solver" 2>/dev/null
        else
            kill -9 "-$solver" 2>/dev/null || kill -9 "$solver" 2>/dev/null
        fi
    else
        kill -9 "$solver" 2>/dev/null
    fi
}

cleanup() {
    if [ -n "$watchdog" ]; then
        kill "$watchdog" 2>/dev/null
        wait "$watchdog" 2>/dev/null
    fi
    if [ -n "$group" ] && [ "$use_setsid" -eq 1 ]; then
        kill -9 "-$group" 2>/dev/null
    elif [ -n "$solver" ]; then
        kill_solver
        wait "$solver" 2>/dev/null
    fi
    rm -rf "$scratch"
}
trap cleanup 0
trap 'exit 2' HUP INT TERM

run_case() {
    number=$1
    dir=$scratch/$number
    mkdir "$dir" || return 2
    write_table "$number" >"$dir/input.table" || return 2
    (
        cd "$dir" || exit 2
        set -- sh -c ': >"$1" && shift && exec "$@"' sh \
            "$scratch/$number.started" sh /workspace/plan.sh input.table
        if [ "$use_setsid" -eq 1 ]; then
            exec setsid "$@"
        fi
        exec "$@"
    ) \
        </dev/null >"$scratch/$number.stdout" 2>"$scratch/$number.stderr" &
    solver=$!
    (
        delay=
        trap 'if [ -n "$delay" ]; then
            kill "$delay" 2>/dev/null
            wait "$delay" 2>/dev/null
        fi' 0
        trap 'exit 0' HUP INT TERM
        # Five seconds for the run, plus one for session launch and the startup marker.
        sleep 6 &
        delay=$!
        wait "$delay" || exit 0
        delay=
        kill -0 "$solver" 2>/dev/null || exit 0
        kill_solver
    ) </dev/null >/dev/null 2>&1 &
    watchdog=$!
    wait "$solver" 2>/dev/null
    status=$? group=$solver solver=
    kill "$watchdog" 2>/dev/null
    wait "$watchdog" 2>/dev/null
    watchdog=
    if [ "$use_setsid" -eq 1 ]; then
        kill -9 "-$group" 2>/dev/null
    fi
    group=
    [ -f "$scratch/$number.started" ] || return 2
    [ "$status" -eq 0 ] && [ -f "$scratch/$number.stdout" ] || return 1
    expected "$number" | cmp -s - "$scratch/$number.stdout" 2>/dev/null
    comparison=$?
    case "$comparison" in
        0) return 0 ;;
        126|127) return 2 ;;
    esac
    [ "$comparison" -le 128 ] || return 2
    return 1
}

write_table() {
    case "$1" in
        1) cat <<'TABLE'
rev a m0
rev b m0
row a m0 idle 0 m0
row a m0 ping 0 m0
row b m0 idle 0 m0
row b m0 ping 1 m0
TABLE
            ;;
        2) cat <<'TABLE'
rev a m0
rev b m0
row a m0 a 0 m0
row a m0 z 0 m0
row b m0 a 0 m0
row b m0 z 0 m1
row b m1 a 1 m1
row b m1 z 0 m1
TABLE
            ;;
        3) cat <<'TABLE'
rev a m0
rev b m0
row a m0 pulse 0 m0
row b m0 pulse 0 m1
row b m1 pulse 0 m2
row b m2 pulse 1 m2
TABLE
            ;;
        4) cat <<'TABLE'
rev a m0
rev b m0
row a m0 idle 0 m0
row a m0 flip 0 m1
row a m0 read 0 m0
row a m1 idle 0 m1
row a m1 flip 0 m0
row a m1 read 0 m1
row b m0 idle 0 m0
row b m0 flip 0 m1
row b m0 read 0 m0
row b m1 idle 0 m1
row b m1 flip 0 m0
row b m1 read 1 m1
TABLE
            ;;
        5) cat <<'TABLE'
rev a m0
rev b m0
rev c m0
rev d m0
row a m0 b 0 m0
row a m0 c 1 m0
row a m0 a 0 m0
row b m0 b 1 m0
row b m0 c 1 m0
row b m0 a 0 m0
row c m0 b 0 m0
row c m0 c 0 m0
row c m0 a 1 m0
row d m0 b 0 m0
row d m0 c 0 m0
row d m0 a 0 m0
TABLE
            ;;
        6) cat <<'TABLE'
rev a m0
rev b m0
rev c m0
rev d m0
row a m0 a 1 m0
row a m0 b 0 m0
row a m0 c 0 m0
row b m0 a 0 m0
row b m0 b 1 m0
row b m0 c 0 m0
row c m0 a 0 m0
row c m0 b 0 m0
row c m0 c 1 m0
row d m0 a 0 m0
row d m0 b 0 m0
row d m0 c 0 m0
TABLE
            ;;
        7) cat <<'TABLE'
rev a m0
rev b m0
rev c m0
row a m0 go 0 m0
row a m0 wait 0 m0
row b m0 go 0 m0
row b m0 wait 0 m0
row c m0 go 1 m0
row c m0 wait 0 m0
TABLE
            ;;
        8) cat <<'TABLE'
rev a m0
rev b m1
row a m0 go 0 m1
row a m0 read 0 m0
row a m1 go 0 m0
row a m1 read 1 m1
row b m0 go 0 m1
row b m0 read 1 m0
row b m1 go 0 m0
row b m1 read 0 m1
TABLE
            ;;
        9) cat <<'TABLE'
rev a m0
rev b m0
row a m0 p2 0 m0
row a m0 p10 1 m0
row b m0 p2 1 m0
row b m0 p10 0 m0
TABLE
            ;;
        10) cat <<'TABLE'
rev a m0
rev b m0
rev c m0
row a m0 z 1 m0
row a m0 read 0 m0
row a m0 a 0 m1
row a m1 z 0 m1
row a m1 read 0 m1
row a m1 a 0 m1
row b m0 z 0 m0
row b m0 read 0 m0
row b m0 a 0 m1
row b m1 z 0 m1
row b m1 read 1 m1
row b m1 a 0 m1
row c m0 z 0 m0
row c m0 read 0 m0
row c m0 a 0 m1
row c m1 z 1 m1
row c m1 read 0 m1
row c m1 a 0 m1
TABLE
            ;;
        11) cat <<'TABLE'
rev a m2
row a m0 probe 0 m1
row a m1 probe 1 m2
row a m2 probe 0 m0
TABLE
            ;;
        12) cat <<'TABLE'
	row	b	m0	kick	0	m0  # line comment
rev a m0
  # whole-line comment
row c m0 kick 0 m0

row a m0 kick 1 m0
row c m0 arc 0 m0
rev c m0
rev b m0
row b m0 arc 1 m0
row a m0 arc 0 m0
TABLE
            ;;
        13) cat <<'TABLE'
rev a m0
rev b m0
rev c m0
rev d m0
rev e m0
row a m0 a 0 m1
row a m0 b 0 m0
row a m0 c 0 m2
row a m1 a 0 m1
row a m1 b 1 m1
row a m1 c 1 m2
row a m2 a 1 m2
row a m2 b 0 m2
row a m2 c 0 m2
row b m0 a 0 m1
row b m0 b 0 m0
row b m0 c 0 m2
row b m1 a 1 m1
row b m1 b 0 m1
row b m1 c 1 m0
row b m2 a 0 m2
row b m2 b 1 m2
row b m2 c 0 m2
row c m0 a 0 m1
row c m0 b 0 m0
row c m0 c 0 m2
row c m1 a 0 m1
row c m1 b 1 m1
row c m1 c 0 m1
row c m2 a 0 m2
row c m2 b 0 m2
row c m2 c 0 m2
row d m0 a 0 m1
row d m0 b 0 m0
row d m0 c 1 m2
row d m1 a 1 m1
row d m1 b 1 m1
row d m1 c 0 m0
row d m2 a 1 m2
row d m2 b 0 m2
row d m2 c 0 m2
row e m0 a 0 m1
row e m0 b 0 m0
row e m0 c 1 m2
row e m1 a 0 m1
row e m1 b 0 m1
row e m1 c 1 m0
row e m2 a 0 m2
row e m2 b 1 m2
row e m2 c 0 m2
TABLE
            ;;
        14) cat <<'TABLE'
rev a m0
rev b m0
row a m0 tick 0 m1
row a m1 tick 0 m2
row a m2 tick 1 m0
row b m0 tick 0 m1
row b m1 tick 0 m2
row b m2 tick 1 m1
TABLE
            ;;
        15) cat <<'TABLE'
rev a m0
rev b m0
rev c m0
row a m0 a 1 m2
row a m0 b 0 m1
row a m1 a 1 m1
row a m1 b 0 m1
row a m2 a 0 m2
row a m2 b 0 m2
row b m0 a 0 m2
row b m0 b 0 m1
row b m1 a 0 m1
row b m1 b 1 m1
row b m2 a 0 m2
row b m2 b 0 m2
row c m0 a 0 m2
row c m0 b 0 m1
row c m1 a 0 m1
row c m1 b 0 m1
row c m2 a 0 m2
row c m2 b 0 m2
TABLE
            ;;
        16) cat <<'TABLE'
rev a m0
rev b m1
row a m0 go 0 m0
row b m0 go 0 m0
row b m1 go 1 m1
TABLE
            ;;
        17) cat <<'TABLE'
rev a m0
rev b m1
row a m0 x 0 m0
row a m0 y 0 m0
row b m1 x 1 m2
row b m1 y 0 m1
row b m2 x 0 m2
row b m2 y 1 m2
TABLE
            ;;
        18) cat <<'TABLE'
rev a m0
rev b m0
rev c m0
rev d m0
row a m0 a 0 m1
row a m0 b 0 m0
row a m0 c 0 m0
row a m1 a 0 m1
row a m1 b 1 m1
row a m1 c 0 m2
row a m2 a 0 m2
row a m2 b 0 m2
row a m2 c 0 m2
row b m0 a 0 m1
row b m0 b 0 m0
row b m0 c 0 m0
row b m1 a 0 m1
row b m1 b 0 m1
row b m1 c 0 m2
row b m2 a 0 m2
row b m2 b 0 m2
row b m2 c 0 m2
row c m0 a 1 m1
row c m0 b 0 m0
row c m0 c 0 m0
row c m1 a 0 m1
row c m1 b 0 m2
row c m1 c 1 m1
row c m2 a 0 m2
row c m2 b 0 m2
row c m2 c 0 m2
row d m0 a 1 m1
row d m0 b 0 m0
row d m0 c 0 m0
row d m1 a 0 m1
row d m1 b 0 m2
row d m1 c 0 m1
row d m2 a 0 m2
row d m2 b 0 m2
row d m2 c 0 m2
TABLE
            ;;
        19) cat <<'TABLE'
rev a m0
rev b m0
row a m0 go0 0 m0
row a m0 go 0 m0
row b m0 go0 1 m0
row b m0 go 1 m0
TABLE
            ;;
        20) cat <<'TABLE'
rev a m0
rev b m0
rev c m0
rev d m0
row a m0 a 0 m1
row a m0 b 0 m0
row a m0 c 0 m0
row a m1 a 0 m1
row a m1 b 1 m1
row a m1 c 0 m1
row b m0 a 0 m1
row b m0 b 0 m0
row b m0 c 1 m0
row b m1 a 0 m1
row b m1 b 0 m1
row b m1 c 0 m1
row c m0 a 1 m1
row c m0 b 1 m0
row c m0 c 0 m0
row c m1 a 0 m1
row c m1 b 0 m1
row c m1 c 1 m1
row d m0 a 1 m1
row d m0 b 1 m0
row d m0 c 1 m0
row d m1 a 0 m1
row d m1 b 0 m1
row d m1 c 0 m1
TABLE
            ;;
        21) cat <<'TABLE'
rev a m0
rev b m0
row a m0 a 1 m0
row a m0 b 0 m0
row b m0 a 1 m0
row b m0 b 0 m0
TABLE
            ;;
        22) cat <<'TABLE'
rev a m0
rev b m0
rev c m0
rev d m0
row a m0 a 0 m0
row a m0 b 0 m1
row a m0 c 0 m0
row a m1 a 0 m1
row a m1 b 0 m1
row a m1 c 1 m1
row b m0 a 0 m0
row b m0 b 0 m1
row b m0 c 0 m0
row b m1 a 0 m1
row b m1 b 0 m1
row b m1 c 0 m1
row c m0 a 1 m0
row c m0 b 1 m1
row c m0 c 0 m0
row c m1 a 0 m1
row c m1 b 0 m1
row c m1 c 1 m1
row d m0 a 1 m0
row d m0 b 1 m1
row d m0 c 0 m0
row d m1 a 0 m1
row d m1 b 0 m1
row d m1 c 0 m1
TABLE
            ;;
        23) cat <<'TABLE'
rev a m0
rev b m0
row a m0 a 1 m0
row a m0 b 1 m1
row a m1 a 0 m1
row a m1 b 1 m1
row b m0 a 1 m0
row b m0 b 1 m1
row b m1 a 1 m1
row b m1 b 1 m1
TABLE
            ;;
        24) cat <<'TABLE'
rev a m0
rev b m0
rev c m0
row a m0 split 0 m0
row a m0 other 0 m0
row b m0 split 1 m0
row b m0 other 0 m0
row c m0 split 1 m0
row c m0 other 1 m0
TABLE
            ;;
        *) return 2 ;;
    esac
}

expected() {
    case "$1" in
        1) printf '%s\n' '1 ping' ;;
        2) printf '%s\n' '2 z' ;;
        3) printf '%s\n' '3 pulse' ;;
        4) printf '%s\n' '2 flip' ;;
        5) printf '%s\n' '2 c' ;;
        6) printf '%s\n' '3 a' ;;
        7) printf '%s\n' 'IMPOSSIBLE' ;;
        8) printf '%s\n' 'IMPOSSIBLE' ;;
        9) printf '%s\n' '1 p10' ;;
        10) printf '%s\n' '3 a' ;;
        11) printf '%s\n' '0 DONE' ;;
        12) printf '%s\n' '2 arc' ;;
        13) printf '%s\n' '3 c' ;;
        14) printf '%s\n' '5 tick' ;;
        15) printf '%s\n' '3 b' ;;
        16) printf '%s\n' '1 go' ;;
        17) printf '%s\n' '1 x' ;;
        18) printf '%s\n' '2 a' ;;
        19) printf '%s\n' '1 go' ;;
        20) printf '%s\n' '2 a' ;;
        21) printf '%s\n' 'IMPOSSIBLE' ;;
        22) printf '%s\n' '2 b' ;;
        23) printf '%s\n' '2 b' ;;
        24) printf '%s\n' '2 other' ;;
        *) return 2 ;;
    esac
}

# Every case starts in order and gets its own five-second watchdog.
passed=0
failed=
number=1
while [ "$number" -le "$n" ]; do
    run_case "$number"
    case $? in
        0) passed=$((passed + 1)) ;;
        1) failed=${failed:+$failed,}$number ;;
        *) broken ;;
    esac
    number=$((number + 1))
done

if [ -z "$failed" ]; then
    printf 'cases %s of %s\n' "$passed" "$n"
    exit 0
fi
printf 'cases %s of %s failed %s\n' "$passed" "$n" "$failed"
exit 1
