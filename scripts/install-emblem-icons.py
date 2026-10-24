#!/usr/bin/env python3
"""Install only managed emblem icons, preserving existing theme configuration."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def atomic(path, data):
    if path.is_symlink():
        raise ValueError('managed icon must not be a symlink')
    if path.exists() and path.read_bytes() == data:
        return False
    fd, name = tempfile.mkstemp(prefix='.mygdrive-icon-', dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as f:
            f.write(data)
            os.fchmod(f.fileno(), 0o644)
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)
    return True


def install(data_home, project):
    theme = Path(data_home) / 'icons/hicolor'
    target = theme / 'scalable/emblems'
    for p in [theme, theme / 'scalable', target]:
        if p.is_symlink():
            raise ValueError('managed theme directory must not be a symlink')
    target.mkdir(parents=True, exist_ok=True)
    changed = False
    index = theme / 'index.theme'
    if not index.exists():
        changed |= atomic(index, Path('/usr/share/icons/hicolor/index.theme').read_bytes())
    for name in ['mygdrive-online', 'mygdrive-offline-ready', 'mygdrive-offline-preparing']:
        changed |= atomic(target / (name + '.svg'), (Path(project) / 'assets/emblems' / (name + '.svg')).read_bytes())
    if changed and shutil.which('gtk-update-icon-cache'):
        subprocess.run(['gtk-update-icon-cache', '-f', str(theme)], check=True, capture_output=True)
    return changed


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--data-home', type=Path, default=Path(os.environ.get('XDG_DATA_HOME', str(Path.home() / '.local/share'))))
    parser.add_argument('--quiet', action='store_true')
    a = parser.parse_args()
    changed = install(a.data_home, Path(__file__).resolve().parent.parent)
    if not a.quiet:
        print('MyGdrive Rust emblem icons installed' if changed else 'MyGdrive Rust emblem icons already installed')
