# Configuration API

All functions live in `lib/config.sh` unless noted.

## `get_cfg KEY`

Prints the value of `KEY`, or nothing when it is not set. This is the one
lookup the rest of the code should use; call it as `get_cfg port`.

## `get_cfgs`

Prints every `KEY=VALUE` pair, one per line. Not to be confused with
get_cfg, which prints one value.

## `get_cfg_path`

Prints the path of the configuration file, `etc/app.conf` unless `APP_CONF`
is set.

## `my_get_cfg KEY` (lib/net.sh)

A hook for local overrides. By default it calls get_cfg.

Accessor names are built from the prefix `get_cfg_` (see `CFG_PREFIX`).
