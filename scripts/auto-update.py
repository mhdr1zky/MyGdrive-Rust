"""Check releases by default; install only after an explicit desktop request."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import stat
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request

REPO = 'mhdr1zky/MyGdrive-Rust'
API = f'https://api.github.com/repos/{REPO}/releases/latest'
INTERVAL = 6 * 60 * 60
RETRY = 30 * 60
MAX_PACKAGE = 1024 * 1024 * 1024


def version(value):
    if not re.fullmatch(r'v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)', value):
        raise ValueError('Versi rilis harus berupa versi stabil x.y.z')
    return tuple(map(int, value.removeprefix('v').split('.')))


def read_state(path):
    try:
        state = json.loads(path.read_text())
        return state if isinstance(state, dict) else {}
    except (OSError, ValueError):
        return {}


def save_state(path, state):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(state, ensure_ascii=False))
    temporary.replace(path)


def fetch(url, limit, destination=None):
    request = urllib.request.Request(url, headers={
        'User-Agent': 'MyGdrive-Rust-Updater',
        'Accept': 'application/vnd.github+json' if url == API else 'application/octet-stream',
    })
    digest = hashlib.sha256()
    content = bytearray()
    total = 0
    deadline = time.monotonic() + 1200
    with urllib.request.urlopen(request, timeout=30) as response:
        if urllib.parse.urlsplit(response.url).scheme != 'https':
            raise ValueError('Unduhan harus melalui HTTPS')
        while block := response.read(1024 * 1024):
            total += len(block)
            if total > limit or time.monotonic() > deadline:
                raise ValueError('Batas ukuran/waktu unduhan terlampaui')
            digest.update(block)
            if destination is None:
                content.extend(block)
            else:
                destination.write(block)
    return bytes(content), digest.hexdigest(), total


def asset_url(release, name):
    matches = [a for a in release['assets'] if a['name'] == name and a.get('state') == 'uploaded']
    if len(matches) != 1:
        raise ValueError(f'Aset rilis belum lengkap: {name}')
    url = matches[0]['browser_download_url']
    expected = f'https://github.com/{REPO}/releases/download/{release["tag_name"]}/{name}'
    if url != expected:
        raise ValueError('Alamat aset bukan milik repositori resmi')
    return url, matches[0]['size']


def checksum(manifest, name):
    matches = []
    for line in manifest.decode('utf-8').splitlines():
        match = re.fullmatch(r'([0-9a-fA-F]{64}) [ *](.+)', line)
        if match and match[2] == name:
            matches.append(match[1].lower())
    if len(matches) != 1:
        raise ValueError('Checksum rilis hilang atau duplikat')
    return matches[0]


def fingerprint(path):
    info = path.stat()
    return [info.st_ino, info.st_size, info.st_mtime_ns]


def install_deb(package, release_version, log_path=None):
    for field, expected in [('Package', 'mygdrive-rust'), ('Version', release_version), ('Architecture', 'amd64')]:
        actual = subprocess.check_output(['/usr/bin/dpkg-deb', '-f', str(package), field], text=True).strip()
        if actual != expected:
            raise ValueError(f'Identitas paket tidak sesuai: {field}')
    # pkexec uses the desktop's authentication agent; never save passwords or sudo rules.
    with (log_path or package.parent / 'install.log').open('w') as log:
        subprocess.run(['/usr/bin/pkexec', '/usr/bin/apt-get', '--yes', '--no-remove',
                        'install', str(package)], check=True, timeout=600,
                       stdout=log, stderr=subprocess.STDOUT)


def perform(kind, target, current, state, status_path, install=False, expected_version=None):
    running = current
    if kind == 'deb':
        current = subprocess.check_output(
            ['/usr/bin/dpkg-query', '-W', '-f=${Version}', 'mygdrive-rust'], text=True).strip()
    elif kind == 'appimage' and state.get('fingerprint') == fingerprint(target):
        current = state.get('installed_version', current)
    state.update(phase='checking', available_version=None, current_version=current,
                 message='Memeriksa rilis GitHub…')
    save_state(status_path, state)
    release = json.loads(fetch(API, 2 * 1024 * 1024)[0])
    if release.get('draft') or release.get('prerelease'):
        raise ValueError('Rilis belum stabil')
    latest = release['tag_name'].removeprefix('v')
    if version(latest) <= version(current):
        restart = version(current) > version(running)
        state.update(phase='installed' if restart else 'current',
                     message=(f'Versi {current} terpasang. Buka ulang aplikasi dan layanan untuk menggunakannya.'
                              if restart else f'Versi {current} sudah terbaru.'),
                     next_check=time.time() + INTERVAL)
        return
    if kind == 'source' or platform.machine() not in ('x86_64', 'amd64'):
        state.update(phase='manual', available_version=latest,
                     message=f'Versi {latest} tersedia. Unduh paket yang sesuai dari halaman rilis.',
                     next_check=time.time() + INTERVAL)
        return
    suffix = 'amd64.deb' if kind == 'deb' else 'x86_64.AppImage'
    name = f'mygdrive-rust_{latest}_{suffix}'
    url, size = asset_url(release, name)
    manifest_url, _ = asset_url(release, 'SHA256SUMS')
    if not 0 < size <= MAX_PACKAGE:
        raise ValueError('Ukuran paket tidak valid')
    state.update(phase='available', available_version=latest,
                 message=f'Versi {latest} tersedia. Anda menentukan kapan memperbarui.',
                 next_check=time.time() + INTERVAL)
    if not install:
        return
    if expected_version != latest:
        raise ValueError('Rilis berubah. Periksa versi yang tersedia lalu tekan Update sekarang lagi.')
    expected = checksum(fetch(manifest_url, 1024 * 1024)[0], name)
    state.update(phase='downloading', message=f'Mengunduh pembaruan {latest}…')
    save_state(status_path, state)
    # AppImage staging must share a filesystem with its destination for atomic replacement.
    directory = target.parent if kind == 'appimage' else status_path.parent
    original = fingerprint(target) if kind == 'appimage' else None
    with tempfile.TemporaryDirectory(prefix='.mygdrive-update-', dir=directory) as work:
        package = Path(work) / name
        with package.open('xb') as output:
            _, actual, downloaded = fetch(url, size, output)
            output.flush()
            os.fsync(output.fileno())
        if downloaded != size or actual != expected:
            raise ValueError('Verifikasi SHA-256/ukuran gagal; paket tidak dipasang')
        state.update(phase='installing', message=f'Memasang pembaruan {latest}…')
        save_state(status_path, state)
        if kind == 'deb':
            install_deb(package, latest, status_path.with_name('install.log'))
        else:
            if target.is_symlink() or fingerprint(target) != original:
                raise ValueError('AppImage berubah selama unduhan; coba lagi nanti')
            mode = stat.S_IMODE(target.stat().st_mode) & 0o777
            package.chmod(mode | stat.S_IXUSR)
            os.replace(package, target)
            state['fingerprint'] = fingerprint(target)
    state.update(phase='installed', available_version=None, installed_version=latest, next_check=time.time() + INTERVAL,
                 message=f'Pembaruan {latest} terpasang. Buka ulang aplikasi untuk memakai versi baru.')


def run(args):
    if os.geteuid() == 0:
        raise ValueError('Jalankan updater sebagai pengguna desktop biasa')
    args.status.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with (args.status.parent / 'update.lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return
        state = read_state(args.status)
        install = getattr(args, 'install', False)
        if not args.force and not install and time.time() < state.get('next_check', 0):
            return
        try:
            perform(args.kind, args.target, args.version, state, args.status,
                    install=install, expected_version=getattr(args, 'expected_version', None))
        except Exception as error:
            state.update(phase='error', available_version=None, next_check=time.time() + RETRY,
                         message=f'Pembaruan belum berhasil: {error}')
            # A dismissed authentication prompt must not reappear every 30 minutes.
            if isinstance(error, subprocess.CalledProcessError):
                state['next_check'] = time.time() + INTERVAL
        state['checked_at'] = time.time()
        save_state(args.status, state)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kind', choices=['deb', 'appimage', 'source'], required=True)
    parser.add_argument('--target', type=Path, required=True)
    parser.add_argument('--version', required=True)
    parser.add_argument('--status', type=Path, required=True)
    parser.add_argument('--force', action='store_true')
    parser.add_argument('--install', action='store_true', help='Install only on explicit user request')
    parser.add_argument('--expected-version', help='Release version selected by the user')
    run(parser.parse_args())
