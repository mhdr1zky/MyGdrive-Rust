#!/usr/bin/env python3
"""Install managed UCA entries, preserving unrelated actions and making a backup."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import shlex
import stat
import tempfile
import uuid
import xml.etree.ElementTree as ET

PREFIX = 'mygdrive-offline-v1-'


def read_json(path, limit=1024 * 1024):
    """Private same-user metadata; never follow a redirected registry/config."""
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'r', encoding='utf-8') as source:
        meta = os.fstat(source.fileno())
        if not stat.S_ISREG(meta.st_mode) or meta.st_uid != os.getuid() or meta.st_mode & 0o022 or meta.st_size > limit:
            raise ValueError('unsafe profile metadata')
        return json.loads(source.read(limit + 1))


def profile(state, account, mount, binary):
    entry = dict(state=str(state), account=account, mount=str(mount), binary=str(binary))
    if not isinstance(account, str) or not account or len(account) > 256:
        raise ValueError('invalid profile account')
    for name in ('state', 'mount', 'binary'):
        path = Path(entry[name])
        if not path.is_absolute() or '..' in path.parts or '\0' in entry[name]:
            raise ValueError('profile paths must be absolute without traversal')
        entry[name] = str(path)
    return entry


def read_profiles(path):
    try:
        data = read_json(path)
    except FileNotFoundError:
        return []
    if not isinstance(data, dict) or data.get('version') != 1 or not isinstance(data.get('profiles'), list) or len(data['profiles']) > 256:
        raise ValueError('invalid profile registry')
    return [profile(**entry) for entry in data['profiles']]


def save_profiles(path, profiles):
    if path.is_symlink():
        raise ValueError('profile registry must not be a symlink')
    fd, temporary = tempfile.mkstemp(prefix='.mygdrive-profiles-', dir=path.parent)
    try:
        with os.fdopen(fd, 'w', encoding='utf-8') as output:
            json.dump(dict(version=1, profiles=profiles), output, ensure_ascii=False)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def select_profiles(profiles, paths):
    """Validate the whole selection before running any command on any profile."""
    resolved = []
    for entry in profiles:
        try:
            config = read_json(Path(entry['state']) / 'service-config.json', 128 * 1024)
            root = Path(config['root'])
            if config['account'] != entry['account'] or not root.is_absolute() or '..' in root.parts:
                raise ValueError('profile configuration mismatch')
        except (OSError, ValueError, KeyError, TypeError):
            root = None
        resolved.append((entry, root))
    groups = {}
    for selected in paths:
        path = Path(selected)
        if not path.is_absolute() or '..' in path.parts or '\0' in str(path):
            raise ValueError('Pilih path absolut tanpa traversal.')
        matches = [(entry, root) for entry, root in resolved
                   if path.is_relative_to(Path(entry['mount'])) or (root is not None and path.is_relative_to(root))]
        if len(matches) != 1 or matches[0][1] is None:
            raise ValueError('Profil pilihan tidak ditemukan atau ambigu. Pasang ulang integrasi profil dan periksa folder yang dipilih.')
        entry, _ = matches[0]
        key = (entry['state'], entry['account'], entry['mount'], entry['binary'])
        groups.setdefault(key, []).append(str(path))
    return [(dict(zip(('state', 'account', 'mount', 'binary'), key)), selected) for key, selected in groups.items()]


def install(config_dir, project, state, account, mount):
    parent = Path(config_dir) / 'Thunar'
    parent.mkdir(parents=True, exist_ok=True)
    if parent.is_symlink():
        raise ValueError('Thunar config directory must not be a symlink')
    # Concurrent installations for different accounts must not lose either entry.
    fd = os.open(parent / '.mygdrive-install.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    with os.fdopen(fd, 'r+') as lock:
        meta = os.fstat(lock.fileno())
        if not stat.S_ISREG(meta.st_mode) or meta.st_uid != os.getuid() or meta.st_mode & 0o022:
            raise ValueError('unsafe integration lock')
        fcntl.flock(lock, fcntl.LOCK_EX)
        return _install(config_dir, project, state, account, mount)


def _install(config_dir, project, state, account, mount):
    binary = Path(project) / 'bin/mygdrive-sync'
    if not binary.is_file():
        binary = Path(project) / 'target/debug/mygdrive-sync'
    parent = Path(config_dir) / 'Thunar'
    parent.mkdir(parents=True, exist_ok=True)
    if parent.is_symlink():
        raise ValueError('Thunar config directory must not be a symlink')
    target = parent / 'uca.xml'
    if target.is_symlink():
        raise ValueError('uca.xml must not be a symlink')
    previous = target.read_bytes() if target.exists() else None
    if previous and len(previous) > 1024 * 1024:
        raise ValueError('uca.xml is too large')
    root = ET.fromstring(previous, parser=ET.XMLParser(target=ET.TreeBuilder(insert_comments=True))) if previous else ET.Element('actions')
    if root.tag != 'actions':
        raise ValueError('unexpected Thunar configuration')
    registry = parent / 'mygdrive-profiles.json'
    profiles = {entry['state']: entry for entry in read_profiles(registry)}
    for node in list(root):
        if node.tag == 'action' and (node.findtext('unique-id') or '').startswith(PREFIX):
            # Migrate the previously installed fixed-profile menu without executing it.
            try:
                argv = shlex.split((node.findtext('command') or '').replace('%%', '%'))
                values = {name: argv[argv.index('--' + name) + 1] for name in ('state', 'account', 'mount', 'binary')}
                old = profile(**values)
                profiles.setdefault(old['state'], old)
            except (ValueError, IndexError):
                pass  # The new dispatcher menu has --registry instead.
            root.remove(node)
    current = profile(state, account, mount, binary)
    profiles[current['state']] = current
    if len(profiles) > 256:
        raise ValueError('too many registered profiles')
    for mode, name, icon, description in [
        ('available', 'Selalu tersedia offline', 'emblem-default', 'Unduh dan pertahankan file/folder MyGdrive Rust untuk penggunaan tanpa internet.'),
        ('unpin', 'Lepaskan pilihan offline', 'weather-clouds', 'Lepaskan pin dan antrekan pembersihan salinan bersih. Perubahan tertunda, file terbuka dan pin lain tetap dilindungi.')]:
        action = ET.SubElement(root, 'action')
        command = ['python3', str(Path(project) / 'scripts/thunar-action.py'), '--registry', str(registry), '--mode', mode, '--']
        # Literal %% protects field-code characters in installation paths.
        text = ' '.join(shlex.quote(s.replace('%', '%%')) for s in command) + ' %F'
        for key, value in [('icon', icon), ('name', name), ('submenu', 'MyGdrive Rust'), ('unique-id', PREFIX + mode), ('command', text), ('description', description), ('range', '1-256'), ('patterns', '*')]:
            ET.SubElement(action, key).text = value
        for kind in ['directories', 'audio-files', 'image-files', 'other-files', 'text-files', 'video-files']:
            ET.SubElement(action, kind)
    data = ET.tostring(root, encoding='utf-8', xml_declaration=True) + b'\n'
    # Stop rather than overwrite a concurrent edit from Thunar's preferences window.
    if (target.read_bytes() if target.exists() else None) != previous:
        raise ValueError('Thunar actions changed during installation; retry')
    save_profiles(registry, list(profiles.values()))
    backup = None
    if previous is not None:
        backup = parent / ('uca.xml.before-mygdrive-' + uuid.uuid4().hex)
        fd = os.open(backup, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'wb') as out:
            out.write(previous)
            out.flush()
            os.fsync(out.fileno())
    fd, temporary = tempfile.mkstemp(prefix='.mygdrive-uca-', dir=parent)
    try:
        with os.fdopen(fd, 'wb') as out:
            out.write(data)
            out.flush()
            os.fsync(out.fileno())
        os.replace(temporary, target)
        directory = os.open(parent, os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    return target, backup


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--config-dir', type=Path, default=Path(os.environ.get('XDG_CONFIG_HOME', Path.home() / '.config')))
    p.add_argument('--project', type=Path, default=Path(__file__).resolve().parent.parent)
    p.add_argument('--state', type=Path, required=True)
    p.add_argument('--account', default='default')
    p.add_argument('--mount', type=Path, required=True)
    a = p.parse_args()
    for path in [a.project, a.state, a.mount]:
        if not path.is_absolute():
            p.error('all installation paths must be absolute')
    target, backup = install(a.config_dir, a.project, a.state, a.account, a.mount)
    print(f'Installed: {target}')
    if backup:
        print(f'Backup: {backup}')


if __name__ == '__main__':
    main()
