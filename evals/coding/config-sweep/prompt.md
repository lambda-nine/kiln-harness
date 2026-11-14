`/workspace/settings.conf` holds about 1,500 settings, one per line, each
written `section.key_N = value`. Lines that start with `#` are comments, and
blank lines separate blocks.

Make exactly these changes to it:

- set `database.key_311` to `4096`;
- set `cache.key_842` to `off`;
- set `auth.key_1204` to `strict`;
- delete the line that sets `queue.key_555`.

A changed setting's line reads `section.key_N = value` with the new value.
Leave every other byte of the file exactly as it is, comments included: a
comment that mentions one of these keys is not a setting. Leave no other
files behind.
