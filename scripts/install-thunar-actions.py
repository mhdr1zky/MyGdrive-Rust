#!/usr/bin/env python3
"""Install managed UCA entries, preserving unrelated actions and making a backup."""
import argparse
import os
from pathlib import Path
import shlex
import tempfile
import uuid
import xml.etree.ElementTree as ET

PREFIX = 'mygdrive-offline-v1-'


def install(config_dir, project, state, account, mount):
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
    for node in list(root):
        if node.tag == 'action' and (node.findtext('unique-id') or '').startswith(PREFIX):
            root.remove(node)
    for mode, name, icon, description in [
        ('available', 'Selalu tersedia offline', 'emblem-default', 'Unduh dan pertahankan file/folder MyGdrive Rust untuk penggunaan tanpa internet.'),
        ('unpin', 'Lepaskan pilihan offline', 'weather-clouds', 'Lepaskan pin. Salinan lokal yang sudah ada tetap dipertahankan; belum membebaskan ruang.')]:
        action = ET.SubElement(root, 'action')
        command = ['python3', str(Path(project) / 'scripts/thunar-action.py'), '--binary', str(binary), '--state', str(state), '--account', account, '--mount', str(mount), '--mode', mode, '--']
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
