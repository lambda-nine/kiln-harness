`/workspace` holds nightly, a set of housekeeping jobs in POSIX `sh`: commands under `bin/`, libraries under `lib/` (some in subdirectories), a config file under `etc/`, and a test script, `tests/run.sh`. Every job logs through `log_event LEVEL MESSAGE`, defined in `lib/log.sh`.

Log lines should say which script wrote them. Make this change:

1. `log_event` takes an optional third argument, COMPONENT. When it is given and not empty, the line reads `LEVEL [COMPONENT]: MESSAGE`; otherwise it reads `LEVEL: MESSAGE` exactly as today. Everything else about `log_event` stays the same: the `LOG_LEVEL` filtering, writing to stderr, and the copy appended to `LOG_FILE`. Update its comment to match. In `lib/log.sh`, change only `log_event` and its comment.

2. Every call to `log_event` in the shell scripts under `bin/` and `lib/` passes a component: the name of the file the call is in, without its directory and without a trailing `.sh`. A call in `lib/net/fetch.sh` passes `fetch`, for example. Write it as one plain unquoted word, added directly after the message argument with one space before it, so `log_event info "fetched $1"` becomes `log_event info "fetched $1" fetch`. When a call continues over several lines, the component still goes right after the message, wherever the message ends; anything that follows the message on the call (a redirection, `;`, `||`, a line continuation and so on) stays where it is, after the component.

Leave these exactly as they are:

- mentions of `log_event` that are not calls: comments (including a commented-out call), text that is printed, such as a usage message or an example inside a here-document, and the docs;
- other names that contain `log_event`, such as `log_events` or `log_event_count`;
- everything outside `bin/` and `lib/`, including `tests/` and `etc/`.

Every other byte stays the same, and no files are added, removed or left behind (no backup or temporary files). `sh tests/run.sh`, run from `/workspace`, fails now on one test that already expects the new format; it must pass when you are done. Commands you run start in `/workspace`; the sandbox has busybox `sh`, `awk`, `sed`, `grep` and the other standard tools, and no network.
