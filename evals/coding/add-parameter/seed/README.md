# nightly

Housekeeping jobs for a small server: backups, log rotation, mirror syncs
and pruning, run by `bin/nightly`.

Every job logs through `log_event LEVEL MESSAGE` from `lib/log.sh`, for
example `log_event warn "disk almost full"`. Set `LOG_LEVEL=debug` to see
everything and `LOG_FILE` to keep a copy of the log.

Run the tests from the project root with `sh tests/run.sh`.
