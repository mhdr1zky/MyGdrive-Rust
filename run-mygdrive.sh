#!/usr/bin/env bash
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
if [[ $# -eq 0 ]]; then
    set -- run
fi
binary_dir="$(python3 "$project_dir/scripts/start-online-drive.py" --source-binary-dir)"
exec "$binary_dir/mygdrive-sync" \
    --state "${XDG_DATA_HOME:-$HOME/.local/share}/mygdrive" \
    --account default "$@"
