# Probe plan

You have one unit whose revision is unknown. Each probe reports one bit
and may change the unit's mode. Always apply probes to the same unit.
There is no reset, and you may repeat any probe as often as you like.

## Table

The table is ASCII text. Records are one per line, with fields separated
by one or more spaces or tabs. Lines end with LF, including the last line.
Leading and trailing spaces and tabs are allowed. A `#` starts a comment
through the end of its line. Ignore blank lines and comment text. The
records may appear in any order.

    rev REV MODE
    row REV MODE PROBE BIT NEXT

- `rev` declares a revision and its initial mode.
- `row` gives the behavior of probe `PROBE` for revision `REV` in mode
  `MODE`. It reports `BIT` (either `0` or `1`) and enters mode `NEXT`.
  `NEXT` may equal `MODE`.
- Revision, mode, and probe names each match `[a-z][a-z0-9]*`. These are
  three separate sets of names, and a name may appear in more than one
  set. The probe names are exactly those that appear in the probe
  column. Mode names appear in the initial, current, or next mode fields.

Input tables are valid. There are one to five revisions, one to three
mode names in the table, and one to three probes. Each revision has one
declaration. Each `row` names a declared revision. For every revision,
each mode reachable from its initial mode by any sequence of probes
has exactly one row per probe. Only rows for reachable modes are
required. Any extra rows are unique by revision, mode, and probe.

## Strategy and answer

Each revision starts in the mode given by its `rev` record. At the start,
every declared revision is possible. A probe gives you its bit and
changes the unit to the row's next mode. The remaining possibilities
are the pairs (revision, current mode) consistent with the probes you
have applied and the bits you have seen. You identify the revision as
soon as just one revision remains possible.

A strategy may choose its next probe based on the full history of bits.
It must identify the revision for every possible starting revision.
A history that cannot occur requires no branch. The strategy's
depth is the largest number of probe applications on any possible history
before identifying the revision. Each application counts, including repeats
of the same probe.

Run `sh /workspace/plan.sh TABLE` from any directory, where `TABLE` is
a path, absolute or relative to that directory. Each invocation can use
its own current directory for temporary files. The grader runs each case
in a separate working directory.

Write exactly one line on stdout, ending with a newline, and exit 0.

- If there is one revision, print `0 DONE`.
- If no finite strategy identifies the revision, print `IMPOSSIBLE`.
- Otherwise print `D P` with one space. `D` is the minimum depth over
  all strategies, written in decimal with no sign or leading zeros. `P`
  is the first probe of a strategy of depth `D`. If multiple first probes
  attain the same minimum depth, choose the one that comes first in ASCII
  lexicographic order.

## Worked examples

### Priming

Initially both probes report 0 for either revision. After `arm`,
`read` reports different bits. The answer is `2 arm`.

```text
rev a m0
rev b m0
row a m0 arm 0 m1
row a m0 read 0 m0
row a m1 arm 0 m1
row a m1 read 0 m1
row b m0 arm 0 m1
row b m0 read 0 m0
row b m1 arm 0 m1
row b m1 read 1 m1
```

### Adaptive choice

Probe `group` gives 0 for revisions a and b, and 1 for c and d.
After 0, use `left`. After 1, use `right`. No other first probe
splits into pairs, and no fixed pair of probes distinguishes all four
revisions. The answer is `2 group`.

```text
rev a m0
rev b m0
rev c m0
rev d m0
row a m0 group 0 m0
row a m0 left 1 m0
row a m0 right 0 m0
row b m0 group 0 m0
row b m0 left 0 m0
row b m0 right 0 m0
row c m0 group 1 m0
row c m0 left 0 m0
row c m0 right 1 m0
row d m0 group 1 m0
row d m0 left 0 m0
row d m0 right 0 m0
```

### Impossible

Each revision stays in its starting mode and always reports 0.
The answer is `IMPOSSIBLE`.

```text
rev a m0
rev b m0
row a m0 read 0 m0
row b m0 read 0 m0
```

### Tie

Either probe identifies the revision immediately. The answer is
`1 amber` because `amber` comes before `zinc`.

```text
rev a m0
rev b m0
row a m0 zinc 0 m0
row a m0 amber 0 m0
row b m0 zinc 1 m0
row b m0 amber 1 m0
```
