# Configuration API

All functions live in `lib/config.sh` unless noted.

## `config_get KEY`

Prints the value of `KEY`, or nothing when it is not set. This is the one
lookup the rest of the code should use; call it as `config_get port`.

## `get_cfgs`

Prints every `KEY=VALUE` pair, one per line. Not to be confused with
config_get, which prints one value.

## `get_cfg_path`

Prints the path of the configuration file, `etc/app.conf` unless `APP_CONF`
is set.

## `my_get_cfg KEY` (lib/net.sh)

A hook for local overrides. By default it calls config_get.

Accessor names are built from the prefix `get_cfg_` (see `CFG_PREFIX`).
