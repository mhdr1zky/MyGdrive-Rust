#!/usr/bin/env bash
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$project_dir"
if [[ -d "$project_dir/.tools/gtk-sdk/root/usr" ]]; then
    export PKG_CONFIG_PATH="$project_dir/.tools/gtk-sdk/root/usr/lib/x86_64-linux-gnu/pkgconfig:$project_dir/.tools/gtk-sdk/root/usr/share/pkgconfig:${PKG_CONFIG_PATH:-}"
fi
if [[ -x "$project_dir/.tools/cargo/bin/cargo" ]]; then
    export RUSTUP_HOME="$project_dir/.tools/rustup"
    export CARGO_HOME="$project_dir/.tools/cargo"
    cargo_bin="$project_dir/.tools/cargo/bin/cargo"
else
    cargo_bin=cargo
fi
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
exec "$cargo_bin" build --features desktop-ui,virtual-drive --bins "$@"
