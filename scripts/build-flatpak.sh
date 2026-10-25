#!/usr/bin/env bash
# Package the same native release binaries used by the Debian build.
# No host helper or sandbox escape is granted. FUSE/Thunar remain native-only.
set -euo pipefail
if [[ $# -gt 1 || ( $# -eq 1 && $1 != --skip-build ) ]]; then
    printf 'Usage: %s [--skip-build]\n' "$0" >&2
    exit 2
fi
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"
for command in flatpak python3 install; do
    command -v "$command" >/dev/null || { printf 'Missing dependency: %s\n' "$command" >&2; exit 1; }
done
version=$(python3 -c 'import pathlib,tomllib; print(tomllib.loads(pathlib.Path("Cargo.toml").read_text())["package"]["version"])')
architecture=$(flatpak --default-arch)
app_id=io.github.mhdr1zky.MyGdrive
runtime=org.gnome.Platform
runtime_version=50
flatpak info --arch="$architecture" "$runtime//$runtime_version" >/dev/null || {
    printf 'Install runtime first: flatpak install --user flathub %s//%s\n' "$runtime" "$runtime_version" >&2
    exit 1
}
if [[ $# -eq 0 ]]; then
    if [[ -x .tools/cargo/bin/cargo ]]; then
        export CARGO_HOME="$project_dir/.tools/cargo" RUSTUP_HOME="$project_dir/.tools/rustup"
        cargo_bin="$CARGO_HOME/bin/cargo"
    else
        cargo_bin=cargo
    fi
    if [[ -d .tools/gtk-sdk/root/usr ]]; then
        export PKG_CONFIG_PATH="$project_dir/.tools/gtk-sdk/root/usr/lib/x86_64-linux-gnu/pkgconfig:$project_dir/.tools/gtk-sdk/root/usr/share/pkgconfig:${PKG_CONFIG_PATH:-}"
    fi
    CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" "$cargo_bin" build --locked --release --all-features --bins
fi
target_dir=${CARGO_TARGET_DIR:-"$project_dir/target"}
[[ $target_dir == /* ]] || target_dir="$project_dir/$target_dir"
if [[ $("$target_dir/release/mygdrive-sync" --version) != "mygdrive-sync $version" ]]; then
    printf 'Release binaries do not match Cargo.toml. Rebuild first.\n' >&2
    exit 1
fi
install -d dist
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/mygdrive-flatpak.XXXXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT
build_dir="$work_dir/app"
# No compilation occurs inside this directory: Platform is also sufficient as
# the packing SDK. Both executables are built from Cargo.lock on Ubuntu 24.04.
flatpak build-init --arch="$architecture" "$build_dir" "$app_id" "$runtime" "$runtime" "$runtime_version"
for binary in mygdrive-sync mygdrive-desktop; do
    install -Dm755 "$target_dir/release/$binary" "$build_dir/files/bin/$binary"
done
install -Dm755 packaging/flatpak/run-desktop.sh "$build_dir/files/bin/mygdrive"
install -Dm644 "packaging/flatpak/$app_id.desktop" "$build_dir/files/share/applications/$app_id.desktop"
install -Dm644 assets/mygdrive.svg "$build_dir/files/share/icons/hicolor/scalable/apps/$app_id.svg"
install -d "$build_dir/files/share/metainfo" "$build_dir/files/share/doc/mygdrive-rust"
python3 - "$version" "$build_dir/files/share/metainfo/$app_id.metainfo.xml" <<'PY'
from pathlib import Path
import sys
source=Path('packaging/flatpak/io.github.mhdr1zky.MyGdrive.metainfo.xml.in').read_text()
Path(sys.argv[2]).write_text(source.replace('@VERSION@',sys.argv[1]))
PY
for document in README.md CHANGELOG.md OAUTH_SETUP.md LICENSE; do
    install -m644 "$document" "$build_dir/files/share/doc/mygdrive-rust/$document"
done
for document in docs/PERFORMANCE.md docs/screenshots/*.png; do
    install -Dm644 "$document" "$build_dir/files/share/doc/mygdrive-rust/$document"
done
flatpak build-finish --command=mygdrive --share=network --share=ipc \
    --socket=wayland --socket=fallback-x11 --device=dri --filesystem=home \
    '--own-name=io.mygdrive.*' --talk-name=org.kde.StatusNotifierWatcher \
    --env=GTK_USE_PORTAL=1 "$build_dir"
# Verify the executables against the actual GNOME runtime before exporting.
flatpak build "$build_dir" /app/bin/mygdrive-sync --version
flatpak build "$build_dir" /app/bin/mygdrive-desktop --help >/dev/null
flatpak build-export --arch="$architecture" "$work_dir/repo" "$build_dir" stable
output="$project_dir/dist/mygdrive-rust_${version}_${architecture}.flatpak"
flatpak build-bundle --arch="$architecture" \
    --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
    "$work_dir/repo" "$output" "$app_id" stable
printf '\nFlatpak bundle: %s\n' "$output"
