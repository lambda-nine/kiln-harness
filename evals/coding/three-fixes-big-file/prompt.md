`/workspace/invoice.awk` is an awk program of about 1,400 lines that turns a CSV file of invoice lines into reports (`awk -f invoice.awk -v report=NAME FILE`). The comment at the top of the file is its manual.

Three bugs have been reported:

1. A customer written in the CSV as `"Acme ""West"" Ltd"` comes out as `Acme West Ltd` in every report. It should be `Acme "West" Ltd`.
2. Refunds come out a cent off. For a file holding only the line
   `A-3,2026-03-03,Acme Corp,CA,general,Widget (returned),-1,19.99,`
   the csv report gives the line a tax of `-1.44`; it should be `-1.45`, the exact mirror of the `1.45` that selling one widget pays.
3. The json report writes a description of `Cable 3\4 adapter` as `"Cable 3\4 adapter"`, which is not valid JSON. It should be `"Cable 3\\4 adapter"`.

Each bug is in a different function. Fix each one in the function that has it, so that every report gives the answer the manual describes for every input, not only for these examples. Change nothing outside those three functions: every other line of the file stays byte for byte as it is. Keep each function's closing `}` on a line of its own at the start of the line.

Leave no other files in `/workspace` (no backup, sample or temporary files; `/tmp` is fine for scratch files). Commands you run start in `/workspace`; the sandbox has busybox `sh`, `awk`, `sed`, `grep` and the other standard tools, and no network.
