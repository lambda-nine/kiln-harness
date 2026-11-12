`/workspace` is a small shell project:

- `bin/app.sh`, the entry point;
- libraries under `lib/`, including subdirectories;
- a test script, `tests/run.sh`;
- `docs/api.md` and `CHANGELOG.md`.

Rename the shell function `get_cfg` to `config_get` throughout the project:

- In every `*.sh` file anywhere under `/workspace`, replace each occurrence of the name `get_cfg` with `config_get`: the definition, every call, and every mention in comments.
- Do the same in `docs/api.md`.
- An occurrence is `get_cfg` as a whole name: the characters directly before and after it (if any) are not letters, digits or `_`. Names that merely contain it, such as `get_cfg_path`, `get_cfgs`, `my_get_cfg` or the text `get_cfg_`, are different names and must stay exactly as they are.
- Leave `CHANGELOG.md` unchanged: it records history.
- Change nothing else. Every byte outside the renamed occurrences stays the same, and no files are added, removed or left behind (no backup or temporary files).

When you are done, `sh tests/run.sh`, run from `/workspace`, must still pass.

Use only POSIX `sh` and the standard tools available here (`awk`, `sed`, `grep`, `find` and so on); there is no network. Commands you run start in `/`, so use absolute paths or `cd /workspace` first, for example `sh -c 'cd /workspace && sh tests/run.sh'`.
