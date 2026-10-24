#!/usr/bin/env bash
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
python3 "$project_dir/scripts/start-online-drive.py" "$@" || true
if [[ -d "$project_dir/.tools/gtk-sdk/root/usr/lib/x86_64-linux-gnu" ]]; then
    export LD_LIBRARY_PATH="$project_dir/.tools/gtk-sdk/root/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
exec "$project_dir/target/debug/mygdrive-desktop" "$@"
