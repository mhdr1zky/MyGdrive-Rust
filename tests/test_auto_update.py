"""Updater tests: no public network, package installation, or desktop authentication."""
import argparse
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location('auto_update', Path(__file__).resolve().parents[1] / 'scripts/auto-update.py')
UPDATER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(UPDATER)
REAL_FETCH = UPDATER.fetch


class UpdateTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.target = self.root / 'application with spaces.AppImage'
        self.target.write_bytes(b'old application')
        self.target.chmod(0o755)
        self.status = self.root / 'updates/status.json'
        self.status.parent.mkdir()
        self.payload = b'new application'
        self.name = 'mygdrive-rust_1.1.0_x86_64.AppImage'
        self.release = dict(tag_name='v1.1.0', draft=False, prerelease=False, assets=[])
        for name, size in [(self.name, len(self.payload)), ('SHA256SUMS', 200)]:
            self.release['assets'].append(dict(name=name, size=size, state='uploaded',
                browser_download_url=f'https://github.com/{UPDATER.REPO}/releases/download/v1.1.0/{name}'))
        self.digest = hashlib.sha256(self.payload).hexdigest()
        self.fetch = mock.patch.object(UPDATER, 'fetch', side_effect=self.fake_fetch).start()
        self.addCleanup(mock.patch.stopall)
        mock.patch.object(UPDATER.platform, 'machine', return_value='x86_64').start()

    def fake_fetch(self, url, limit, destination=None):
        if url == UPDATER.API:
            return json.dumps(self.release).encode(), '', 0
        if url.endswith('/SHA256SUMS'):
            return f'{self.digest}  {self.name}\n'.encode(), '', 0
        if destination:
            destination.write(self.payload)
            return b'', hashlib.sha256(self.payload).hexdigest(), len(self.payload)
        raise AssertionError(url)

    def perform(self, state=None):
        state = {} if state is None else state
        UPDATER.perform('appimage', self.target, '1.0.2', state, self.status)
        return state

    def test_atomic_appimage_update_keeps_path_permissions_and_cleans_staging(self):
        state = self.perform()
        self.assertEqual(self.target.read_bytes(), self.payload)
        self.assertEqual(self.target.stat().st_mode & 0o777, 0o755)
        self.assertEqual(state['installed_version'], '1.1.0')
        self.assertFalse(list(self.root.glob('.mygdrive-update-*')))
        self.fetch.reset_mock()
        self.perform(state)
        self.assertEqual(self.fetch.call_count, 1)  # old running binary does not reinstall

    def test_corrupt_download_preserves_original(self):
        self.digest = '0' * 64
        with self.assertRaisesRegex(ValueError, 'SHA-256'):
            self.perform()
        self.assertEqual(self.target.read_bytes(), b'old application')
        self.assertFalse(list(self.root.glob('.mygdrive-update-*')))

    def test_interrupted_download_preserves_original_and_cleans_staging(self):
        fetch = self.fake_fetch
        def interrupted(url, limit, destination=None):
            if destination:
                destination.write(b'incomplete')
                raise OSError('disconnected')
            return fetch(url, limit, destination)
        self.fetch.side_effect = interrupted
        with self.assertRaises(OSError):
            self.perform()
        self.assertEqual(self.target.read_bytes(), b'old application')
        self.assertFalse(list(self.root.glob('.mygdrive-update-*')))

    def test_same_older_and_prerelease_are_never_installed(self):
        for tag in ('v1.0.2', 'v1.0.1'):
            self.release['tag_name'] = tag
            self.fetch.reset_mock()
            self.perform()
            self.assertEqual(self.fetch.call_count, 1)
        self.release['prerelease'] = True
        with self.assertRaisesRegex(ValueError, 'stabil'):
            self.perform()

    def test_numeric_version_and_reject_unsafe_tags(self):
        self.assertGreater(UPDATER.version('v1.10.0'), UPDATER.version('1.9.0'))
        for tag in ('1.0.0-beta', '../evil', '1.0', '01.2.3'):
            with self.assertRaises(ValueError):
                UPDATER.version(tag)

    def test_incomplete_release_and_foreign_asset_are_rejected(self):
        self.release['assets'].pop()
        with self.assertRaisesRegex(ValueError, 'lengkap'):
            self.perform()
        self.release['assets'][0]['browser_download_url'] = 'https://example.com/payload'
        with self.assertRaisesRegex(ValueError, 'resmi'):
            self.perform()

    def test_missing_duplicate_checksum_and_size_mismatch_are_rejected(self):
        for manifest in (b'', (f'{self.digest}  {self.name}\n' * 2).encode()):
            with self.assertRaises(ValueError):
                UPDATER.checksum(manifest, self.name)
        self.release['assets'][0]['size'] += 1
        with self.assertRaisesRegex(ValueError, 'ukuran'):
            self.perform()

    def test_read_only_target_failure_preserves_original(self):
        replace = UPDATER.os.replace
        def deny_target(source, destination):
            if Path(destination) == self.target:
                raise PermissionError('read only')
            return replace(source, destination)
        with mock.patch.object(UPDATER.os, 'replace', side_effect=deny_target):
            with self.assertRaises(PermissionError):
                self.perform()
        self.assertEqual(self.target.read_bytes(), b'old application')

    def test_target_changed_during_download_is_not_overwritten(self):
        fetch = self.fake_fetch
        def concurrent_change(url, limit, destination=None):
            result = fetch(url, limit, destination)
            if destination:
                self.target.write_bytes(b'manually updated by user')
            return result
        self.fetch.side_effect = concurrent_change
        with self.assertRaisesRegex(ValueError, 'berubah'):
            self.perform()
        self.assertEqual(self.target.read_bytes(), b'manually updated by user')

    def test_deb_identity_checked_before_authentication(self):
        with mock.patch.object(UPDATER.subprocess, 'check_output', return_value='other-package'), \
                mock.patch.object(UPDATER.subprocess, 'run') as install:
            with self.assertRaisesRegex(ValueError, 'Identitas'):
                UPDATER.install_deb(self.root / 'file.deb', '1.1.0')
            install.assert_not_called()

    def test_deb_authentication_uses_argument_array(self):
        with mock.patch.object(UPDATER.subprocess, 'check_output', side_effect=['mygdrive-rust', '1.1.0', 'amd64']), \
                mock.patch.object(UPDATER.subprocess, 'run') as install:
            package = self.root / 'package with spaces.deb'
            UPDATER.install_deb(package, '1.1.0')
            self.assertEqual(install.call_args.args[0], ['/usr/bin/pkexec', '/usr/bin/apt-get', '--yes', '--no-remove', 'install', str(package)])

    def test_failure_records_retry_and_throttles_next_launch(self):
        args = argparse.Namespace(kind='appimage', target=self.target, version='1.0.2', status=self.status, force=False)
        with mock.patch.object(UPDATER.os, 'geteuid', return_value=1000):
            self.fetch.side_effect = OSError('offline')
            UPDATER.run(args)
            state = UPDATER.read_state(self.status)
            self.assertIn('offline', state['message'])
            self.assertGreater(state['next_check'], UPDATER.time.time())
            self.fetch.reset_mock()
            UPDATER.run(args)
            self.fetch.assert_not_called()

    def test_concurrent_profile_does_not_start_second_update(self):
        args = argparse.Namespace(kind='appimage', target=self.target, version='1.0.2', status=self.status, force=False)
        with mock.patch.object(UPDATER.os, 'geteuid', return_value=1000), \
                mock.patch.object(UPDATER.fcntl, 'flock', side_effect=BlockingIOError):
            UPDATER.run(args)
            self.fetch.assert_not_called()

    def test_authentication_cancel_waits_six_hours(self):
        args = argparse.Namespace(kind='deb', target=self.target, version='1.0.2', status=self.status, force=False)
        with mock.patch.object(UPDATER.os, 'geteuid', return_value=1000), \
                mock.patch.object(UPDATER, 'perform', side_effect=subprocess.CalledProcessError(126, ['pkexec'])):
            UPDATER.run(args)
        state = UPDATER.read_state(self.status)
        self.assertGreater(state['next_check'], UPDATER.time.time() + UPDATER.INTERVAL - 5)
        self.assertIn('belum terpasang', state['message'])

    def test_malformed_state_can_recover(self):
        for content in ('invalid', '[]', 'null'):
            self.status.write_text(content)
            self.assertEqual(UPDATER.read_state(self.status), {})

    def test_fetch_enforces_limit(self):
        response = io.BytesIO(b'oversized')
        response.url = 'https://github.com/example'
        with mock.patch.object(UPDATER.urllib.request, 'urlopen', return_value=response):
            with self.assertRaisesRegex(ValueError, 'Batas'):
                REAL_FETCH('https://github.com/example', 3)


if __name__ == '__main__':
    unittest.main()
