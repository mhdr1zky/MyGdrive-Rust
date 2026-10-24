#!/usr/bin/env bash
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# Compatibility launcher: follow the saved profile after scope migration.
exec "$project_dir/run-mygdrive.sh" "$@"
