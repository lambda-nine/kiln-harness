# dus

Small disk-usage helpers for POSIX `sh`. A scan file holds one `PATH BYTES`
line per file; `bin/dus.sh` summarizes it.

- `lib/size.sh`: `fmt_size` (powers of 1024) and `fmt_size_si` (powers of
  1000) turn a byte count into text such as `1.5K` or `1.5kB`.
- `lib/parse.sh`: `parse_size`, the inverse of `fmt_size`.
- `lib/report/`: the summary, top and diff reports.
- `share/hist.awk`: a standalone histogram program.

Run the tests from the project root with `sh tests/run.sh`.
