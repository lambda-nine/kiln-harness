#!/bin/sh
LC_ALL=C
export LC_ALL
exec awk '
# Give each mode a digit. A state has one digit per revision, with x for
# a revision already ruled out. Probe names start with a letter.
function mode_id(name) {
    if (!(name in mode_number))
        mode_number[name] = ++mode_count
    return mode_number[name]
}

{ sub(/#.*/, "") }
$1 == "rev" {
    revision[++revision_count] = $2
    initial[$2] = mode_id($3)
}
$1 == "row" {
    if (!($4 in seen_probe)) {
        seen_probe[$4] = 1
        probe[++probe_count] = $4
    }
    key = $2 SUBSEP mode_id($3) SUBSEP $4
    bit[key] = $5
    next_mode[key] = mode_id($6)
}

function intern(state, size) {
    if (!(state in state_id)) {
        state_id[state] = ++state_count
        states[state_count] = state
        sizes[state_count] = size
    }
    return state_id[state]
}

# Build one transition for each observation that can occur.
function expand(state_id_here, name,    state, i, m, k, a, b, na, nb) {
    state = states[state_id_here]
    for (i = 1; i <= revision_count; i++) {
        m = substr(state, i, 1)
        k = revision[i] SUBSEP m SUBSEP name
        if (m != "x" && bit[k] == 0) {
            a = a next_mode[k]
            na++
        } else
            a = a "x"
        if (m != "x" && bit[k] == 1) {
            b = b next_mode[k]
            nb++
        } else
            b = b "x"
    }
    if (na)
        zero[state_id_here, name] = intern(a, na)
    if (nb)
        one[state_id_here, name] = intern(b, nb)
}

END {
    if (revision_count == 1) {
        print "0 DONE"
        exit
    }
    for (i = 1; i <= revision_count; i++)
        start = start initial[revision[i]]
    root = intern(start, revision_count)

    for (s = 1; s <= state_count; s++) {
        if (sizes[s] == 1) {
            depth[s] = 0
            continue
        }
        for (j = 1; j <= probe_count; j++)
            expand(s, probe[j])
    }

    # Layer d consists of states with a probe whose children all have
    # depth below d. Only earlier layers count, even during this scan.
    for (d = 1; ; d++) {
        changed = 0
        for (s = 1; s <= state_count; s++) {
            if (s in depth)
                continue
            first = ""
            for (j = 1; j <= probe_count; j++) {
                p = probe[j]
                z = zero[s, p]
                o = one[s, p]
                if (!z || !o)
                    continue
                if ((z && (!(z in depth) || depth[z] >= d)) ||
                    (o && (!(o in depth) || depth[o] >= d)))
                    continue
                if (first == "" || p < first)
                    first = p
            }
            if (first != "") {
                if (s == root) {
                    print d, first
                    exit
                }
                depth[s] = d
                changed = 1
            }
        }
        if (!changed) {
            print "IMPOSSIBLE"
            exit
        }
    }
}
' < "$1"
