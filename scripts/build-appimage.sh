#!/usr/bin/env bash
set -euo pipefail
[[ $# -eq 0 || ( $# -eq 1 && $1 == --skip-build ) ]] || { echo "Usage: $0 [--skip-build]" >&2; exit 2; }
project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"
[[ $(uname -m) == x86_64 ]] || { echo 'AppImage currently supports x86_64 only' >&2; exit 1; }
version=$(python3 -c 'import pathlib,tomllib;print(tomllib.loads(pathlib.Path("Cargo.toml").read_text())["package"]["version"])')
if [[ $# -eq 0 ]]; then
    if [[ -x .tools/cargo/bin/cargo ]]; then
        export CARGO_HOME="$project_dir/.tools/cargo" RUSTUP_HOME="$project_dir/.tools/rustup"
        cargo_bin="$CARGO_HOME/bin/cargo"
    else cargo_bin=cargo; fi
    if [[ -d .tools/gtk-sdk/root/usr ]]; then
        export PKG_CONFIG_PATH="$project_dir/.tools/gtk-sdk/root/usr/lib/x86_64-linux-gnu/pkgconfig:$project_dir/.tools/gtk-sdk/root/usr/share/pkgconfig"
    fi
    CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" "$cargo_bin" build --locked --release --all-features --bins
fi
target_dir=${CARGO_TARGET_DIR:-"$project_dir/target"}
[[ $target_dir == /* ]] || target_dir="$project_dir/$target_dir"
[[ $("$target_dir/release/mygdrive-sync" --version) == "mygdrive-sync $version" ]] || { echo 'Rebuild release binaries first' >&2; exit 1; }
install -d .tools/appimage dist
fetch() {
    local output=$1 url=$2 digest=$3
    if [[ ! -f $output ]] || ! printf '%s  %s\n' "$digest" "$output" | sha256sum --check --status; then
        curl --fail --location --retry 3 "$url" --output "$output"
    fi
    printf '%s  %s\n' "$digest" "$output" | sha256sum --check
}
fetch .tools/appimage/appimagetool-1.9.1.AppImage https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
fetch .tools/appimage/runtime-20251108-x86_64 https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64 2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
chmod +x .tools/appimage/appimagetool-1.9.1.AppImage
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/mygdrive-appimage.XXXXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT
app_dir="$work_dir/MyGdrive.AppDir"
install -d "$app_dir/usr/bin" "$app_dir/usr/share/applications" "$app_dir/usr/share/doc/mygdrive-rust"
for binary in mygdrive-sync mygdrive-desktop mygdrive-mount; do
    install -m755 "$target_dir/release/$binary" "$app_dir/usr/bin/$binary"
done
install -m755 packaging/appimage/AppRun "$app_dir/AppRun"
install -m644 packaging/appimage/mygdrive.desktop "$app_dir/mygdrive.desktop"
install -m644 packaging/appimage/mygdrive.desktop "$app_dir/usr/share/applications/mygdrive.desktop"
install -m644 assets/mygdrive.svg "$app_dir/mygdrive.svg"
ln -s mygdrive.svg "$app_dir/.DirIcon"
for document in README.md CHANGELOG.md OAUTH_SETUP.md LICENSE docs/PERFORMANCE.md docs/screenshots/*.png; do
    install -Dm644 "$document" "$app_dir/usr/share/doc/mygdrive-rust/$document"
done
install -d "$app_dir/usr/share/metainfo"
python3 - "$version" "$app_dir/usr/share/metainfo/io.github.mhdr1zky.MyGdrive.metainfo.xml" <<'PYMETA'
import pathlib,sys
pathlib.Path(sys.argv[2]).write_text(pathlib.Path('packaging/appimage/mygdrive.appdata.xml.in').read_text().replace('@VERSION@',sys.argv[1]))
PYMETA
env -u LD_LIBRARY_PATH python3 scripts/package-appimage-deps.py "$app_dir"
# Validate the portable entrypoints before making a single-file image.
"$app_dir/AppRun" --sync --version
"$app_dir/AppRun" --help >/dev/null
"$app_dir/AppRun" --mount --help >/dev/null
output="$project_dir/dist/mygdrive-rust_${version}_x86_64.AppImage"
ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 .tools/appimage/appimagetool-1.9.1.AppImage \
    --runtime-file "$project_dir/.tools/appimage/runtime-20251108-x86_64" "$app_dir" "$output"
APPIMAGE_EXTRACT_AND_RUN=1 "$output" --sync --version
APPIMAGE_EXTRACT_AND_RUN=1 "$output" --help >/dev/null
APPIMAGE_EXTRACT_AND_RUN=1 "$output" --mount --help >/dev/null
printf '\nAppImage: %s\n' "$output"
