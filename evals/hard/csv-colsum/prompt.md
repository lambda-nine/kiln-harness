Write a POSIX shell script `/workspace/colsum.sh` that sums one column of a CSV file. It will be run as `sh /workspace/colsum.sh FILE COLUMN`, from any directory, and must print the sum of the integer values in the column whose header is exactly `COLUMN` (case-sensitive), followed by a single LF. Use only POSIX `sh` and the standard tools available here (`awk`, `sed`, `sort`, `grep` and so on); there is no network.

## CSV format (RFC 4180)

- The file is a sequence of records. The first record is the header; every later record is a data record. Record numbers count from 1 for the header and count records, not lines.
- Fields are separated by `,`. A field may be enclosed in double quotes. Inside a quoted field, `""` stands for one `"`, and `,`, CR and LF are ordinary characters, so one record can span several lines.
- Records end with LF or CRLF (the CR is not part of the last field). The last record may have no line ending at all.
- Header fields may be quoted too; compare the header's value (without the quotes) with `COLUMN`. If several headers match, use the first one.

## Values

- A value in the chosen column, after removing any quotes, is an optional `-` or `+` followed by one or more decimal digits, with nothing else (no spaces). An empty field counts as 0.
- Values in other columns are not checked.
- Sums stay within plus or minus 2^53. Print the sum as a plain decimal integer (for example `-7` or `9007199254740991`).

## Exit status

Check the data records in order and stop at the first problem:

| Situation | Exit | Output |
|---|---|---|
| success | 0 | the sum on stdout |
| `FILE` missing or unreadable | 2 | nothing on stdout |
| no header equal to `COLUMN` | 3 | nothing on stdout |
| a data record's field count differs from the header's | 5 | nothing on stdout |
| a bad value in the chosen column | 4 | nothing on stdout; exactly one line `bad value in record N` on stderr |

Within one record, a wrong field count is reported before a bad value.

For example, `sh /workspace/colsum.sh /workspace/sales.csv units` prints `39`. Leave `sales.csv` unchanged. Commands you run start in `/workspace`.
