#!/usr/bin/env python3
"""Publish public project sources through an isolated Git checkout, without AI."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from urllib.parse import urlsplit


PROJECT = Path(__file__).resolve().parents[1]
DEFAULT_REPO = 'https://github.com/mhdr1zky/MyGdrive-Rust.git'
SOURCE_DIRS = {'.cargo', '.github', 'assets', 'docs', 'migrations', 'packaging', 'scripts', 'src', 'tests'}
SOURCE_FILES = {'.gitignore', '.gitattributes', '.editorconfig', 'Cargo.toml', 'Cargo.lock',
                'LICENSE', 'rust-toolchain.toml', 'rustfmt.toml', 'clippy.toml'}
PRIVATE_NAMES = {'credentials.json', 'oauth-client.json', 'token.json', 'tokens.json'}
STATE_FILE = 'mygdrive-publish-state.json'


class PublishError(Exception):
    pass


def git(directory, *arguments, data=None):
    env = os.environ.copy()
    for key in ('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_COMMON_DIR'):
        env.pop(key, None)
    env['GIT_TERMINAL_PROMPT'] = '0'
    result = subprocess.run(['git', '-C', str(directory), *arguments], input=data,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    if result.returncode:
        detail = result.stderr.decode(errors='replace').strip()
        raise PublishError(detail or 'Perintah Git gagal.')
    return result.stdout


def public_path(relative):
    path = Path(relative)
    parts = path.parts
    if not parts or path.is_absolute() or '..' in parts or '.git' in parts:
        return False
    if any(part in {'.tools', 'target', 'dist', 'artifacts', '.aws', '.codex', '.agents', 'oauth', '__pycache__'} for part in parts):
        return False
    name = path.name.lower()
    if (name in PRIVATE_NAMES or name == '.env' or name.startswith('.env.')
            or name.startswith('client_secret') or name.endswith(('.token', '.pem', '.key', '.sqlite3', '.sqlite3-wal', '.sqlite3-shm', '.log', '.part', '.deb', '.appimage'))):
        return False
    return (len(parts) > 1 and parts[0] in SOURCE_DIRS) or (
        len(parts) == 1 and (name.endswith('.md') or relative in SOURCE_FILES
                            or (name.endswith('.sh') and name.startswith(('run-', 'build-')))))


def fingerprint(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(256 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest(), bool(path.stat().st_mode & 0o111)


def inventory():
    # An empty temporary index lets Git interpret the source's .gitignore even
    # when this folder was downloaded/exported without its Git metadata.
    with tempfile.TemporaryDirectory(prefix='mygdrive-source-index-') as temporary:
        git(temporary, 'init', '--quiet')
        result = git(PROJECT, '--git-dir', str(Path(temporary) / '.git'),
                     '--work-tree', str(PROJECT), 'ls-files', '--others', '--exclude-standard', '-z')
    files = {}
    for raw in result.split(b'\0'):
        relative = os.fsdecode(raw)
        if not public_path(relative):
            continue
        source = PROJECT / relative
        if source.is_symlink() or any(p.is_symlink() for p in source.parents if p != PROJECT and PROJECT in p.parents):
            raise PublishError(f'Symlink sumber perlu diperiksa: {relative}')
        if not source.is_file():
            raise PublishError(f'Sumber bukan berkas biasa: {relative}')
        files[relative] = fingerprint(source)
    for required in ('Cargo.toml', 'Cargo.lock', '.gitignore', 'src/lib.rs'):
        if required not in files:
            raise PublishError(f'Sumber proyek tidak lengkap: {required}')
    return files


def tree(checkout, revision):
    entries = {}
    for row in git(checkout, 'ls-tree', '-r', '-z', revision).split(b'\0'):
        if row:
            header, name = row.split(b'\t', 1)
            mode, kind, oid = header.decode().split()
            if kind == 'blob':
                entries[os.fsdecode(name)] = [mode, oid]
    return entries


def read_state(checkout):
    path = checkout / '.git' / STATE_FILE
    if not path.exists():
        return {}
    state = json.loads(path.read_text())
    if not isinstance(state, dict) or any(not public_path(name) for name in state):
        raise PublishError('Manifest publikasi lokal tidak valid.')
    return state


def target_path(checkout, relative):
    path = checkout / relative
    for parent in path.parents:
        if parent == checkout:
            break
        if parent.is_symlink():
            raise PublishError(f'Direktori tujuan berupa symlink: {relative}')
    return path


def prepare(checkout, repo, branch):
    if not (checkout / '.git').is_dir():
        if checkout.exists():
            raise PublishError(f'Checkout tidak lengkap; periksa {checkout}')
        git(PROJECT, 'clone', '--quiet', '--single-branch', '--branch', branch, '--', repo, str(checkout))
    if git(checkout, 'remote', 'get-url', 'origin').decode().strip() != repo:
        raise PublishError('Remote checkout berbeda dari tujuan publikasi.')
    if git(checkout, 'branch', '--show-current').decode().strip() != branch:
        raise PublishError('Branch checkout berbeda dari tujuan publikasi.')
    if git(checkout, 'status', '--porcelain', '-z'):
        raise PublishError(f'Checkout memiliki perubahan yang belum selesai. Periksa: {checkout}')
    git(checkout, 'fetch', '--quiet', '--no-tags', 'origin', branch)
    # Pending commits from a failed push remain available. Diverged histories
    # stop here, before any source overlay; no force push or automatic rebase.
    git(checkout, 'merge', '--ff-only', 'FETCH_HEAD')


def stage(checkout, files, previous):
    remote = tree(checkout, 'FETCH_HEAD')
    removed = set(previous) - set(files)
    for relative in set(files) | removed:
        if relative not in previous or remote.get(relative) == previous[relative]:
            continue
        if relative in removed and relative not in remote:
            continue
        destination = target_path(checkout, relative)
        # Accept identical edits made independently, otherwise preserve server
        # work for manual reconciliation instead of silently overwriting it.
        if (relative in files and destination.is_file() and not destination.is_symlink()
                and fingerprint(destination) == files[relative]):
            continue
        raise PublishError(f'Perubahan GitHub bertabrakan dengan sumber lokal: {relative}')
    for relative, expected in files.items():
        source = PROJECT / relative
        destination = target_path(checkout, relative)
        if destination.is_dir() and not destination.is_symlink():
            raise PublishError(f'Tujuan berkas sudah berupa direktori: {relative}')
        if destination.is_symlink():
            destination.unlink()
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
        if fingerprint(destination) != expected:
            raise PublishError(f'Sumber berubah saat disalin; ulangi setelah diperiksa: {relative}')
    for relative in removed:
        destination = target_path(checkout, relative)
        if destination.is_file() or destination.is_symlink():
            destination.unlink()
    git(checkout, 'add', '--all')
    # .gitignore alone is insufficient for files already tracked upstream.
    # Check the actual publication diff as a second boundary.
    for raw in git(checkout, 'diff', '--cached', '--name-only', '--diff-filter=ACMR', '-z').split(b'\0'):
        if raw and not public_path(os.fsdecode(raw)):
            raise PublishError('Perubahan publikasi mencakup berkas di luar kode publik.')
    git(checkout, 'diff', '--cached', '--check')


def publish(checkout, args, files, previous):
    prepare(checkout, args.repo, args.branch)
    stage(checkout, files, previous)
    summary = git(checkout, 'diff', '--cached', '--stat').decode(errors='replace').strip()
    print(summary or 'Tidak ada perubahan berkas.', flush=True)
    if args.dry_run:
        print('Pratinjau selesai; belum ada commit atau push.')
        return
    if summary:
        git(checkout, 'var', 'GIT_AUTHOR_IDENT')
        git(checkout, 'commit', '-m', args.message)
    # Explicit refspec and no --force. This also retries a previous failed push
    # when no new file changes have appeared.
    git(checkout, 'push', '--porcelain', 'origin', f'HEAD:refs/heads/{args.branch}')
    current = tree(checkout, 'HEAD')
    state = {name: current[name] for name in files if name in current}
    manifest = checkout / '.git' / STATE_FILE
    temporary = manifest.with_suffix('.tmp')
    temporary.write_text(json.dumps(state, indent=2) + '\n')
    temporary.replace(manifest)
    commit = git(checkout, 'rev-parse', '--short', 'HEAD').decode().strip()
    print(f'Berhasil dikirim ke branch {args.branch}: {commit}')
    print('GitHub Actions menjalankan pemeriksaan sesuai workflow repositori.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('message', nargs='?', default='chore: update MyGdrive sources', help='Pesan commit')
    parser.add_argument('--repo', default=DEFAULT_REPO, help='URL repositori tujuan')
    parser.add_argument('--branch', default='main', help='Branch tujuan (default: main)')
    parser.add_argument('--dry-run', action='store_true', help='Tampilkan perubahan tanpa commit/push')
    parser.add_argument('--cache', type=Path, default=PROJECT / 'artifacts/github-publish', help='Direktori checkout kerja')
    args = parser.parse_args()
    if not shutil.which('git'):
        raise PublishError('Git belum terpasang.')
    if urlsplit(args.repo).password or (urlsplit(args.repo).scheme in ('http', 'https') and urlsplit(args.repo).username):
        raise PublishError('Gunakan credential helper Git atau SSH; URL repositori harus tanpa token.')
    git(PROJECT, 'check-ref-format', '--branch', args.branch)
    files = inventory()
    key = hashlib.sha256(f'{args.repo}\0{args.branch}'.encode()).hexdigest()[:16]
    cache = args.cache.resolve()
    if cache == PROJECT or (PROJECT in cache.parents and cache.relative_to(PROJECT).parts[0] not in {'artifacts', '.tools'}):
        raise PublishError('Letakkan checkout kerja di artifacts, .tools, atau di luar direktori sumber.')
    cache.mkdir(parents=True, exist_ok=True, mode=0o700)
    checkout = cache / key
    with (cache / f'{key}.lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise PublishError('Publikasi ke tujuan ini masih berjalan.') from error
        previous = read_state(checkout)
        if args.dry_run:
            with tempfile.TemporaryDirectory(prefix='preview-', dir=cache) as temporary:
                publish(Path(temporary) / 'repo', args, files, previous)
        else:
            publish(checkout, args, files, previous)


if __name__ == '__main__':
    try:
        main()
    except (PublishError, OSError, ValueError) as error:
        print(f'Publikasi dihentikan: {error}', file=sys.stderr)
        sys.exit(1)
