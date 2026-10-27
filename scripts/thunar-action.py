#!/usr/bin/env python3
"""Thunar expands %F into separate argv; never interpret file names as shell code."""
import argparse
import importlib.util
from pathlib import Path
import subprocess
import shutil


def profiles_module():
    spec = importlib.util.spec_from_file_location('mygdrive_thunar_profiles', Path(__file__).with_name('install-thunar-actions.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def dispatch(arguments):
    if not 1 <= len(arguments.paths) <= 256:
        raise ValueError('Pilih 1–256 file/folder MyGdrive Rust.')
    if arguments.registry:
        if any((arguments.binary, arguments.state, arguments.mount)):
            raise ValueError('registry cannot be combined with a fixed profile')
        registry = profiles_module()
        groups = registry.select_profiles(registry.read_profiles(Path(arguments.registry)), arguments.paths)
    else:
        if not all((arguments.binary, arguments.state, arguments.mount)):
            raise ValueError('binary, state and mount are required for a fixed profile')
        groups = [(dict(binary=arguments.binary, state=arguments.state,
                        account=arguments.account, mount=arguments.mount), arguments.paths)]
    for profile, paths in groups:
        result = subprocess.run([profile['binary'], '--state', profile['state'], '--account', profile['account'],
                                 'offline-path', '--mode', arguments.mode, '--mount', profile['mount'],
                                 '--', *paths], capture_output=True, text=True)
        if result.returncode:
            raise ValueError(result.stderr.strip() or 'Pilihan gagal. Periksa apakah layanan MyGdrive Rust aktif.')


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--registry')
    p.add_argument('--binary')
    p.add_argument('--state')
    p.add_argument('--account', default='default')
    p.add_argument('--mount')
    p.add_argument('--mode', choices=['available', 'unpin'], required=True)
    p.add_argument('paths', nargs='+')
    a = p.parse_args()
    try:
        dispatch(a)
    except (OSError, ValueError, TypeError, KeyError) as error:
        message = str(error)
        if shutil.which('zenity'):
            subprocess.run(['zenity', '--error', '--no-markup', '--title=MyGdrive Rust', '--text', message[:2000]])
        elif shutil.which('notify-send'):
            subprocess.run(['notify-send', '--app-name=MyGdrive Rust', 'MyGdrive Rust', message[:2000]])
        return 1
    if shutil.which('notify-send'):
        message = ('Pilihan offline disimpan. Unduhan menunggu koneksi dan layanan yang tidak dijeda.'
                   if a.mode == 'available' else 'Pilihan offline dilepas. Pembersihan salinan bersih menunggu siklus online; perubahan tertunda dan file terbuka tetap dilindungi.')
        subprocess.run(['notify-send', '--app-name=MyGdrive Rust', 'MyGdrive Rust', message])
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
