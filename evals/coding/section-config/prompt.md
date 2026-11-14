`/workspace/app.ini` holds the settings for every environment, about 2,400 lines in 224 sections. Its format:

- A section starts with a line `[name]` and runs to the next such line. Names are matched exactly: `[prod.db]`, `[prod.db.replica]` and `[prod-eu.db]` are three different sections.
- A setting is a line `key = value`; the spacing around `=` varies from section to section.
- A line that starts with spaces continues the value of the setting above it, one item per line. A setting written `key =` with nothing after `=` takes its whole value from the continuation lines that follow it.
- Lines that start with `#` or `;` are comments, and blank lines separate sections.

Most keys appear in many sections, often with the same values. Make exactly these changes, each only in the section named:

1. In `[prod.db]`, set `pool_size` to `40`.
2. In `[prod.db.replica]`, add the setting `max_lag_seconds = 30`, written exactly like that, as a new line directly after the `hosts` setting, that is, after its last continuation line.
3. In `[dev.cache]`, delete the `servers` setting together with all of its continuation lines.
4. Rename the section `[staging.web]` to `[staging.frontend]`. Change only its `[name]` line; `[staging.web.tls]` and comments that mention either section stay as they are.
5. In `[prod.web]`, remove `https://admin.example.com` from `allowed_origins`, and add `https://shop.example.com` as its last item, indented like the other items.
6. In `[canary.api]`, set `timeout` to `45`.
7. In `[dr.scheduler]`, remove the job `30 3 * * * vacuum` from `jobs`, keeping the other jobs in order.

When you change a value, change only the value: the key, the spacing around `=` and everything else on the line stay as they are. Comments are not settings and never change, even where they mention a key or a section. Leave every other byte of the file exactly as it is, and leave no other files behind.
