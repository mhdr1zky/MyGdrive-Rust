#!/usr/bin/env bash
# Build the native-architecture Debian package; never installs it on the host.
set -euo pipefail
if [[ $# -ne 0 ]]; then
    printf 'Usage: %s\n' "$0" >&2
    exit 2
fi
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"
for command in python3 dpkg dpkg-deb dpkg-shlibdeps install; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'Missing build dependency: %s\n' "$command" >&2
        exit 1
    fi
done
if [[ -n ${CARGO_BUILD_TARGET:-} ]]; then
    printf 'build-deb.sh supports native builds; unset CARGO_BUILD_TARGET.\n' >&2
    exit 1
fi
if [[ -x "$project_dir/.tools/cargo/bin/cargo" ]]; then
    export RUSTUP_HOME="$project_dir/.tools/rustup"
    export CARGO_HOME="$project_dir/.tools/cargo"
    export PATH="$CARGO_HOME/bin:$PATH"
    cargo_bin="$CARGO_HOME/bin/cargo"
else
    cargo_bin=cargo
fi
if [[ -d "$project_dir/.tools/gtk-sdk/root/usr" ]]; then
    export PKG_CONFIG_PATH="$project_dir/.tools/gtk-sdk/root/usr/lib/x86_64-linux-gnu/pkgconfig:$project_dir/.tools/gtk-sdk/root/usr/share/pkgconfig:${PKG_CONFIG_PATH:-}"
fi
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
"$cargo_bin" build --locked --release --all-features --bins

version=$(python3 -c 'import pathlib, tomllib; print(tomllib.loads(pathlib.Path("Cargo.toml").read_text())["package"]["version"])')
dpkg --validate-version "$version"
architecture=$(dpkg --print-architecture)
target_dir=${CARGO_TARGET_DIR:-"$project_dir/target"}
if [[ $target_dir != /* ]]; then
    target_dir="$project_dir/$target_dir"
fi
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/mygdrive-deb.XXXXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT
package_dir="$work_dir/debian/mygdrive-rust"
app_dir="$package_dir/usr/lib/mygdrive-rust"
install -d "$package_dir/DEBIAN" "$app_dir/bin" "$app_dir/scripts" "$app_dir/assets/emblems" \
    "$package_dir/usr/bin" "$package_dir/usr/share/applications" \
    "$package_dir/usr/share/icons/hicolor/scalable/apps" \
    "$package_dir/usr/share/icons/hicolor/scalable/emblems" \
    "$package_dir/usr/share/doc/mygdrive-rust"
for binary in mygdrive-sync mygdrive-desktop mygdrive-mount; do
    install -m 0755 "$target_dir/release/$binary" "$app_dir/bin/$binary"
done
install -m 0755 packaging/debian/run-desktop.sh "$app_dir/run-desktop.sh"
ln -s ../lib/mygdrive-rust/run-desktop.sh "$package_dir/usr/bin/mygdrive-desktop"
for binary in mygdrive-sync mygdrive-mount; do
    ln -s "../lib/mygdrive-rust/bin/$binary" "$package_dir/usr/bin/$binary"
done
for script in configure-online-drive.py install-emblem-icons.py install-thunar-actions.py start-online-drive.py thunar-action.py; do
    install -m 0644 "scripts/$script" "$app_dir/scripts/$script"
done
install -m 0644 assets/mygdrive.svg "$package_dir/usr/share/icons/hicolor/scalable/apps/mygdrive.svg"
for icon in assets/emblems/*.svg; do
    install -m 0644 "$icon" "$app_dir/assets/emblems/$(basename "$icon")"
    install -m 0644 "$icon" "$package_dir/usr/share/icons/hicolor/scalable/emblems/$(basename "$icon")"
done
install -m 0644 packaging/debian/io.mygdrive.Desktop.desktop "$package_dir/usr/share/applications/io.mygdrive.Desktop.desktop"
install -m 0644 LICENSE "$package_dir/usr/share/doc/mygdrive-rust/copyright"
for document in README.md OAUTH_SETUP.md CHANGELOG.md LICENSE; do
    install -m 0644 "$document" "$package_dir/usr/share/doc/mygdrive-rust/$document"
done
for document in docs/PERFORMANCE.md docs/screenshots/*.png; do
    install -Dm0644 "$document" "$package_dir/usr/share/doc/mygdrive-rust/$document"
done
for hook in postinst postrm; do
    install -m 0755 "packaging/debian/$hook" "$package_dir/DEBIAN/$hook"
done
cp packaging/debian/control "$work_dir/debian/control"
dependencies=$(cd "$work_dir" && dpkg-shlibdeps -O \
    -e"$app_dir/bin/mygdrive-sync" \
    -e"$app_dir/bin/mygdrive-desktop" \
    -e"$app_dir/bin/mygdrive-mount")
dependencies=${dependencies#shlibs:Depends=}
python3 - "$package_dir" "$version" "$architecture" "$dependencies" <<'PY'
import hashlib
from pathlib import Path
import sys

root, version, architecture, dependencies = Path(sys.argv[1]), *sys.argv[2:]
template = Path('packaging/debian/control').read_text().split('\n\n', 1)[1]
template = template.replace('Architecture: any', f'Architecture: {architecture}')
template = template.replace('@DEPENDS@', dependencies)
size = sum(path.lstat().st_size for path in root.rglob('*') if path.is_file())
control = f'Version: {version}\nInstalled-Size: {(size + 1023) // 1024}\n' + template
# Source-only fields do not carry over to the binary package stanza.
control = control.replace('Package: mygdrive-rust\n', 'Package: mygdrive-rust\nSection: net\nPriority: optional\nMaintainer: MyGdrive Rust contributors\n')
(root / 'DEBIAN/control').write_text(control)
(root / 'DEBIAN/control').chmod(0o644)
checksums = []
for path in sorted(root.rglob('*')):
    relative = path.relative_to(root)
    if relative.parts[0] == 'DEBIAN' or not path.is_file() or path.is_symlink():
        continue
    checksums.append(f'{hashlib.md5(path.read_bytes()).hexdigest()}  {relative}\n')
(root / 'DEBIAN/md5sums').write_text(''.join(checksums))
(root / 'DEBIAN/md5sums').chmod(0o644)
PY
install -d "$project_dir/dist"
output="$project_dir/dist/mygdrive-rust_${version}_${architecture}.deb"
dpkg-deb --root-owner-group --build "$package_dir" "$output"
printf '\nDebian package: %s\n' "$output"
