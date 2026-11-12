Write a POSIX shell script `/workspace/rollup.sh` that summarizes log files by UTC hour. It will be run as `sh /workspace/rollup.sh DIR`, from any directory. Use only POSIX `sh` and the standard tools available here (`awk`, `sed`, `sort`, `grep` and so on); there is no network.

## Which files

Read every regular file directly inside `DIR` whose name ends in `.log`, including names that start with a dot. Do not descend into subdirectories, and ignore every other file (for example `x.txt` or `x.log.1`). Files are LF-terminated text; a file's last line may have no LF.

## Line format

A line is valid when it is exactly `TIMESTAMP LEVEL` or `TIMESTAMP LEVEL MESSAGE`, separated by single spaces:

- `TIMESTAMP` is `YYYY-MM-DDTHH:MM:SS` followed by either `Z` or an offset `+HH:MM` or `-HH:MM`. The date must be a real Gregorian calendar date with a year from 2000 to 2099 (leap years included), the time must be from `00:00:00` to `23:59:59`, and the offset's minutes must be `00` to `59`, with the whole offset from `-12:00` to `+14:00`.
- `LEVEL` is exactly one of `DEBUG`, `INFO`, `WARN`, `ERROR` (upper case).
- `MESSAGE` is one or more characters of any kind. If there is a space after `LEVEL`, a message must follow it.
- A line containing a carriage return anywhere is malformed, so CRLF line endings make lines malformed.

A blank line (empty, or only spaces and tabs) is ignored. Every other line that is not valid is malformed.

## Output

Convert each valid line's timestamp to UTC (subtract the offset: `10:00:00+02:00` is `08:00:00Z`). The date can change, across a month or year boundary too, and the UTC year may then be 1999 or 2100. Count the valid lines by UTC hour and level, then print:

1. For each UTC hour with at least one valid line, in ascending order, one line `YYYY-MM-DDTHH debug=N info=N warn=N error=N` (the hour's date and two-digit hour, then the four counts).
2. Last, one line `malformed=N`, the number of malformed lines in all files read.

Every line ends with LF. With no `.log` files or no valid lines, the output is just the `malformed=N` line. Exit 0.

For example, `sh /workspace/rollup.sh /workspace/logs` must print:

```text
2026-03-14T04 debug=0 info=0 warn=0 error=1
2026-03-14T09 debug=2 info=3 warn=1 error=1
2026-03-14T10 debug=0 info=2 warn=1 error=0
2026-03-14T13 debug=0 info=0 warn=0 error=1
2026-03-14T23 debug=0 info=1 warn=0 error=0
2026-03-15T00 debug=0 info=1 warn=1 error=1
malformed=5
```

Leave `/workspace/logs` unchanged. Commands you run start in `/`, so use absolute paths or `cd /workspace` first.
