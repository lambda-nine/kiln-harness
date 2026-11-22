A factory has one unit of unknown revision. Its table describes what
each probe reports and how it changes the unit's mode.

Implement `/workspace/plan.sh` so it can be run from any directory as

```sh
sh /workspace/plan.sh TABLE
```

Use BusyBox sh and awk. Read `/workspace/docs/probe.md` for the input
format, strategy rules, and exact output. Worked tables and their
answers are also in `/workspace/examples/`.

Minimize the worst-case number of probe applications, counting repeats.
Choose the ASCII-smallest first probe among strategies attaining that
minimum.

Keep `docs/` and `examples/` unchanged.
The checker runs your program once for each of its tests, and each run must finish within 5 seconds.
