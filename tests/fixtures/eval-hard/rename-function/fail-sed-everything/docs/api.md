# Configuration API

All functions live in `lib/config.sh` unless noted.

## `config_get KEY`

Prints the value of `KEY`, or nothing when it is not set. This is the one
lookup the rest of the code should use; call it as `config_get port`.

## `config_gets`

Prints every `KEY=VALUE` pair, one per line. Not to be confused with
config_get, which prints one value.

## `config_get_path`

Prints the path of the configuration file, `etc/app.conf` unless `APP_CONF`
is set.

## `my_config_get KEY` (lib/net.sh)

A hook for local overrides. By default it calls config_get.

Accessor names are built from the prefix `config_get_` (see `CFG_PREFIX`).
