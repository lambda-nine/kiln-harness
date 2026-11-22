#!/bin/sh
LC_ALL=C
export LC_ALL
exec awk '
# Use revision@mode lists and bounded recursive strategy search. Each
# memo entry bounds the depth for which that list can be identified.
{ sub(/#.*/, "") }
$1 == "rev" {
    count++
    state = state $2 "@" $3 " "
    modes[$2, $3] = 1
}
$1 == "row" {
    label[$4] = 1
    modes[$2, $3] = modes[$2, $6] = 1
    result[$2 "@" $3, $4] = $5
    arrival[$2 "@" $3, $4] = $2 "@" $6 " "
}

function branch(position, action,    n, pairs, i, unit, low, high) {
    n = split(position, pairs, " ")
    for (i = 1; i <= n; i++) {
        unit = pairs[i]
        if (result[unit, action] == "0")
            low = low arrival[unit, action]
        else
            high = high arrival[unit, action]
    }
    return low "|" high
}

function works(position, action, remaining,    children, parts) {
    children = branch(position, action)
    split(children, parts, /[|]/)
    return can_finish(parts[1], remaining) && can_finish(parts[2], remaining)
}

function can_finish(position, remaining,    action) {
    # A trailing space marks the end of each pair.
    if (position == "" || position ~ /^[^ ]+ $/)
        return 1
    if ((position in wins) && remaining >= wins[position])
        return 1
    if (!remaining || ((position in losses) && remaining <= losses[position]))
        return 0

    for (action in label) {
        if (works(position, action, remaining - 1)) {
            wins[position] = remaining
            return 1
        }
    }
    losses[position] = remaining
    return 0
}

END {
    if (count == 1) {
        print "0 DONE"
        exit
    }

    # Each revision is ruled out or occupies one of its listed modes.
    # Subtract the empty state and the states with just one revision.
    for (key in modes) {
        split(key, fields, SUBSEP)
        choices[fields[1]]++
    }
    limit = 1
    for (rev in choices) {
        limit *= 1 + choices[rev]
        singletons += choices[rev]
    }
    limit -= 1 + singletons

    for (depth = 1; depth <= limit; depth++) {
        best = ""
        for (probe_name in label) {
            if (works(state, probe_name, depth - 1) &&
                (best == "" || probe_name < best))
                best = probe_name
        }
        if (best != "") {
            print depth, best
            exit
        }
        losses[state] = depth
    }
    print "IMPOSSIBLE"
}
' < "$1"
