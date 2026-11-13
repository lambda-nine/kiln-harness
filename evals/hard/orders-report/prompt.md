`/workspace` holds a quarter's sales data and the rules for its report.

## Files

- `data/orders.csv`: one order per line.
- `data/returns.csv`: one return per line.
- `data/sku_aliases.txt`: alternative spellings of SKUs.
- `docs/REPORT_RULES.md`: the rules that define the report.
- `NOTES.md`: a colleague's notes from an earlier report.

## Task

Produce `/workspace/report.txt` exactly as `docs/REPORT_RULES.md` defines it, byte for byte.

- Read every rule carefully. The rules are the authority over any notes.
- Do not modify anything under `data/` or `docs/`.
- Only `report.txt` is judged, so you may delete any scratch files you make.

Use any of the standard tools available here (`awk`, `sort`, `sed`, `grep` and so on) to compute the report; there is no network. Commands you run start in `/workspace`.
