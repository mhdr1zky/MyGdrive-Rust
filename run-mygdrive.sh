#!/usr/bin/env bash
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
if [[ $# -eq 0 ]]; then
    set -- run --interval 30
fi
exec "$project_dir/target/debug/mygdrive-sync" \
    --state "${XDG_DATA_HOME:-$HOME/.local/share}/mygdrive" \
    --account default "$@"
