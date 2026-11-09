#!/usr/bin/env bash
# Usage: scripts/sandbox.sh <name> <command> [args...]
#
# Runs a command with no HERDR_* variables and private XDG folders under
# /tmp/itm-<name>, so neither the command nor any Herdr it starts can reach
# the real Herdr sessions or itm's real state. The path is short on purpose:
# Herdr's socket paths are limited to about 108 bytes (DESIGN.md, section 1).
# Desktop notifications go to /tmp/itm-<name>/notifications.log.
# /tmp is shared: a root someone else made (or could write to) is refused,
# since the command runs with its HOME, config and notify.sh.
# ITM (the itm to run) passes through.
set -euo pipefail
name=$1; shift
root=/tmp/itm-$name
mkdir -m 700 "$root" 2>/dev/null || true
if [ -L "$root" ] || [ ! -d "$root" ] || [ ! -O "$root" ] || [ "$(stat -c %a "$root")" != 700 ]; then
  echo "refusing $root: not a folder of yours with mode 700 (remove it and run again)" >&2
  exit 2
fi
mkdir -p "$root"/config "$root"/state "$root"/data "$root"/runtime "$root"/home
chmod 700 "$root"/runtime
notify=$root/notify.sh
printf '#!/bin/sh\nprintf "%%s | %%s\\n" "$1" "$2" >> %s/notifications.log\n' "$root" > "$notify"
chmod +x "$notify"
exec env -i \
  PATH="$PATH" \
  HOME="$root/home" \
  USER="${USER:-$(id -un)}" \
  TERM="${TERM:-xterm-256color}" \
  LANG="${LANG:-C.UTF-8}" \
  XDG_CONFIG_HOME="$root/config" \
  XDG_STATE_HOME="$root/state" \
  XDG_DATA_HOME="$root/data" \
  XDG_RUNTIME_DIR="$root/runtime" \
  ITM_NOTIFY_COMMAND="$notify" \
  ITM_SANDBOX="$root" \
  ${ITM:+"ITM=$ITM"} \
  "$@"
