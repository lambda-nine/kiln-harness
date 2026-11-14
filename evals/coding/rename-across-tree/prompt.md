`/workspace` holds dus, a small set of disk-usage helpers in POSIX `sh`: commands under `bin/` and `contrib/`, libraries under `lib/` (some in subdirectories), a standalone awk program in `share/`, docs, and a test script, `tests/run.sh`.

Rename the shell function `fmt_size`, defined in `lib/size.sh`, to `format_size`. In the `.sh` files, change the name wherever it refers to that shell function:

- its definition;
- every call, wherever it appears: at the start of a line or after `;`, `&&` or `||`, in a pipeline, inside `$( )` or backquotes (also when those sit inside a double-quoted string or a here-document), and inside a shell command string passed to `sh -c`;
- places that name the function without calling it right there, such as `command -v fmt_size` or a variable assigned the function's name so it can be called through the variable later;
- comments that mention the function.

Keep everything else exactly as it is:

- What the programs print must not change. Where `fmt_size` is part of literal text that is printed or compared, such as an error message, a usage text, a `printf` format or a test's label or expected value, it stays `fmt_size`.
- Names that merely contain `fmt_size`, such as `fmt_size_si`, `fmt_size_list` or `fmt_sizes`, are different names. A shell variable that is itself called `fmt_size` is not the function either and keeps its name.
- awk programs, whether in their own file or embedded in a shell script, define their own `fmt_size` helper; those are not the shell function and stay as they are.
- Files that are not `.sh` files (`README.md`, `CHANGELOG.md`, `docs/`, `etc/`, `share/` and the test fixtures) stay unchanged.

Every other byte stays the same, and no files are added, removed or left behind (no backup or temporary files). `sh tests/run.sh`, run from `/workspace`, passes now and must still pass afterwards. Commands you run start in `/workspace`; the sandbox has busybox `sh`, `awk`, `sed`, `grep` and the other standard tools, and no network.
