#!/usr/bin/env python3
"""Thunar expands %F into separate argv; never interpret file names as shell code."""
import argparse
import subprocess
import shutil


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--binary', required=True)
    p.add_argument('--state', required=True)
    p.add_argument('--account', default='default')
    p.add_argument('--mount', required=True)
    p.add_argument('--mode', choices=['available', 'unpin'], required=True)
    p.add_argument('paths', nargs='+')
    a = p.parse_args()
    result = subprocess.run([a.binary, '--state', a.state, '--account', a.account,
                             'offline-path', '--mode', a.mode, '--mount', a.mount,
                             '--', *a.paths], capture_output=True, text=True)
    if result.returncode:
        message = result.stderr.strip() or 'Pilihan gagal. Periksa apakah layanan MyGdrive Rust aktif.'
        if shutil.which('zenity'):
            subprocess.run(['zenity', '--error', '--no-markup', '--title=MyGdrive Rust', '--text', message[:2000]])
        elif shutil.which('notify-send'):
            subprocess.run(['notify-send', '--app-name=MyGdrive Rust', 'MyGdrive Rust', message[:2000]])
        return result.returncode
    if shutil.which('notify-send'):
        message = ('Pilihan offline disimpan. Unduhan menunggu koneksi dan layanan yang tidak dijeda.'
                   if a.mode == 'available' else 'Pilihan offline dilepas. Salinan lokal yang sudah ada tetap disimpan.')
        subprocess.run(['notify-send', '--app-name=MyGdrive Rust', 'MyGdrive Rust', message])
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
