#!/bin/sh
LC_ALL=C
export LC_ALL
exec awk '
# Breadth-first search of fixed probe sequences. In addition to current
# modes, keep the partition of revisions sharing the same bit history.
{ sub(/#.*/, "") }
$1 == "rev" {
    rev[++n] = $2
    initial[$2] = $3
}
$1 == "row" {
    seen[$4] = 1
    observation[$2, $3, $4] = $5
    change[$2, $3, $4] = $6
}

END {
    if (n == 1) {
        print "0 DONE"
        exit
    }
    # Enqueue actions in ASCII order so BFS also selects the first probe.
    for (p in seen) {
        i = ++pcount
        while (i > 1 && p < ordered[i - 1]) {
            ordered[i] = ordered[i - 1]
            i--
        }
        ordered[i] = p
    }

    for (i = 1; i <= n; i++) {
        mode_list[1] = mode_list[1] initial[rev[i]] " "
        groups[1] = groups[1] "1"
    }
    visited[mode_list[1], groups[1]] = 1
    tail = 1
    for (head = 1; head <= tail; head++) {
        split(mode_list[head], current_mode, " ")
        for (j = 1; j <= pcount; j++) {
            p = ordered[j]
            new_modes = new_groups = ""
            new_count = 0
            for (i = 1; i <= n; i++) {
                bit = observation[rev[i], current_mode[i], p]
                bucket = substr(groups[head], i, 1) bit
                if (!(bucket in next_group))
                    next_group[bucket] = ++new_count
                new_groups = new_groups next_group[bucket]
                new_modes = new_modes change[rev[i], current_mode[i], p] " "
            }
            for (bucket in next_group)
                delete next_group[bucket]

            first = (head == 1 ? p : first_probe[head])
            if (new_count == n) {
                print depth[head] + 1, first
                exit
            }
            if (!((new_modes, new_groups) in visited)) {
                visited[new_modes, new_groups] = 1
                mode_list[++tail] = new_modes
                groups[tail] = new_groups
                depth[tail] = depth[head] + 1
                first_probe[tail] = first
            }
        }
    }
    print "IMPOSSIBLE"
}
' < "$1"
