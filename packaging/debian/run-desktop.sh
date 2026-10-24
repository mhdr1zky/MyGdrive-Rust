#!/bin/sh
set -eu
app_dir=$(CDPATH= cd -- "$(dirname -- "$(readlink -f -- "$0")")" && pwd)
python3 "$app_dir/scripts/start-online-drive.py" "$@" || true
exec "$app_dir/bin/mygdrive-desktop" "$@"
