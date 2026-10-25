#!/usr/bin/env python3
"""Start the configured online mount when the desktop application opens."""
import json
import os
from pathlib import Path
import subprocess
import sys


NAMES = ('mygdrive-desktop', 'mygdrive-sync', 'mygdrive-mount')


def executable(path):
    return path.is_file() and os.access(path, os.X_OK)


def source_binary_dir(project):
    candidates = []
    for profile in ('release', 'debug'):
        directory = project / 'target' / profile
        binaries = [directory / name for name in NAMES]
        if all(executable(binary) for binary in binaries):
            # The oldest member dates the last complete build, avoiding a
            # mixture of new binaries and old ones while Cargo is still busy.
            candidates.append((min(binary.stat().st_mtime_ns for binary in binaries), profile == 'release', directory))
    return max(candidates, key=lambda candidate: candidate[:2])[2] if candidates else project / 'target/debug'


def mount_binary(project):
    packaged = project / 'bin/mygdrive-mount'
    if executable(packaged):
        return packaged
    selected = os.environ.get('MYGDRIVE_SOURCE_BIN_DIR')
    directory = Path(selected) if selected else source_binary_dir(project)
    if directory not in (project / 'target/release', project / 'target/debug'):
        raise ValueError('invalid source binary directory')
    binary = directory / 'mygdrive-mount'
    if not executable(binary):
        raise ValueError('build virtual-drive support first')
    return binary


def start(arguments):
    # Snapshot rendering must remain observational.
    if any(a == '--snapshot' or a.startswith('--snapshot=') for a in arguments):
        return
    state = Path(os.environ.get('XDG_DATA_HOME', str(Path.home() / '.local/share'))) / 'mygdrive'
    for i, arg in enumerate(arguments):
        if arg == '--state' and i + 1 < len(arguments):
            state = Path(arguments[i + 1])
        elif arg.startswith('--state='):
            state = Path(arg.split('=', 1)[1])
    config_path = state / 'virtual-drive.json'
    if not config_path.exists():
        return
    if config_path.is_symlink() or config_path.stat().st_size > 16384:
        raise ValueError('unsafe online-drive configuration')
    config = json.loads(config_path.read_text())
    if config.get('version') != 1:
        raise ValueError('unsupported online-drive configuration')
    mount = Path(config['mount'])
    if not mount.is_absolute():
        raise ValueError('mount must be absolute')
    if os.path.ismount(mount):
        return
    project = Path(__file__).resolve().parent.parent
    # A one-time icon install; subsequent starts only compare three small SVGs.
    # Snapshot mode returns before this and must stay observational.
    subprocess.run([sys.executable, str(project / 'scripts/install-emblem-icons.py'), '--quiet'], check=True)
    binary = mount_binary(project)
    log = os.open(state / 'online-drive.log', os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        child = subprocess.Popen([str(binary), '--state', str(state), '--account', config['account'], '--mount', str(mount)], stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True, close_fds=True)
    finally:
        os.close(log)
    # Retain the Popen object only until this short-lived launcher exits. The mount owns its lock.
    return child.pid


if __name__ == '__main__':
    try:
        if sys.argv[1:] == ['--source-binary-dir']:
            print(source_binary_dir(Path(__file__).resolve().parent.parent))
        else:
            result = start(sys.argv[1:])
            if result:
                print(f'MyGdrive Rust online mount starting (PID {result})', file=sys.stderr)
    except (OSError, ValueError, KeyError) as error:
        print(f'MyGdrive Rust online mount: {error}', file=sys.stderr)
        raise SystemExit(1)
