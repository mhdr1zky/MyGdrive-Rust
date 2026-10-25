#!/usr/bin/env bash
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
binary_dir="$(python3 "$project_dir/scripts/start-online-drive.py" --source-binary-dir)"
# Keep the mount on this same build if another build finishes during startup.
MYGDRIVE_SOURCE_BIN_DIR="$binary_dir" python3 "$project_dir/scripts/start-online-drive.py" "$@" || true
if [[ -d "$project_dir/.tools/gtk-sdk/root/usr/lib/x86_64-linux-gnu" ]]; then
    export LD_LIBRARY_PATH="$project_dir/.tools/gtk-sdk/root/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
exec "$binary_dir/mygdrive-desktop" "$@"
