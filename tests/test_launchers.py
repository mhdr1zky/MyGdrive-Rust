"""Run with python3 -m unittest discover -s tests -p test_launchers.py."""
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock


PROJECT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('online_launcher', PROJECT / 'scripts/start-online-drive.py')
LAUNCHER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(LAUNCHER)
NAMES = ('mygdrive-desktop', 'mygdrive-sync', 'mygdrive-mount')


def binary(project, directory, name, marker):
    path = project / directory / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f'#!/bin/sh\nprintf \'%s\\n\' {marker} "$@"\n')
    path.chmod(0o700)
    return path


class LauncherTests(unittest.TestCase):
    def assert_source_profile(self, release_names, expected, stale_release=False):
        with tempfile.TemporaryDirectory(prefix='mygdrive launcher ') as directory:
            project = Path(directory)
            (project / 'scripts').mkdir()
            shutil.copyfile(PROJECT / 'run-desktop.sh', project / 'run-desktop.sh')
            shutil.copyfile(PROJECT / 'run-mygdrive.sh', project / 'run-mygdrive.sh')
            shutil.copyfile(PROJECT / 'scripts/start-online-drive.py', project / 'scripts/start-online-drive.py')
            for name in NAMES:
                path = binary(project, 'target/debug', name, 'debug')
                os.utime(path, ns=(200, 200))
            for name in release_names:
                path = binary(project, 'target/release', name, 'release')
                timestamp = 100 if stale_release else 300
                os.utime(path, ns=(timestamp, timestamp))
            result = subprocess.run(
                ['bash', str(project / 'run-desktop.sh'), '--snapshot', 'preview with spaces.png'],
                capture_output=True, text=True, check=True,
                env={**os.environ, 'XDG_DATA_HOME': str(project / 'data')},
            )
            self.assertEqual(result.stdout.splitlines(), [expected, '--snapshot', 'preview with spaces.png'])
            result = subprocess.run(
                ['bash', str(project / 'run-mygdrive.sh')],
                capture_output=True, text=True, check=True,
                env={**os.environ, 'XDG_DATA_HOME': str(project / 'data')},
            )
            self.assertEqual(result.stdout.splitlines(), [expected, '--state', str(project / 'data/mygdrive'), '--account', 'default', 'run'])
            with mock.patch.dict(os.environ, {}, clear=True):
                self.assertEqual(LAUNCHER.mount_binary(project), project / f'target/{expected}/mygdrive-mount')

    def test_complete_release_build_is_used_for_desktop_service_and_mount(self):
        self.assert_source_profile(NAMES, 'release')

    def test_debug_fallback_keeps_partial_release_builds_from_mixing_profiles(self):
        for release_names in [(), ('mygdrive-desktop', 'mygdrive-mount'), ('mygdrive-desktop', 'mygdrive-sync')]:
            with self.subTest(release_names=release_names):
                self.assert_source_profile(release_names, 'debug')

    def test_a_stale_complete_release_does_not_replace_a_newer_debug_build(self):
        self.assert_source_profile(NAMES, 'debug', stale_release=True)

    def test_mount_uses_the_profile_chosen_before_another_build_finished(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            for name in NAMES:
                binary(project, 'target/debug', name, 'debug')
                binary(project, 'target/release', name, 'release')
            selected = project / 'target/debug'
            with mock.patch.dict(os.environ, {'MYGDRIVE_SOURCE_BIN_DIR': str(selected)}):
                self.assertEqual(LAUNCHER.mount_binary(project), selected / 'mygdrive-mount')

    def test_packaged_mount_takes_precedence_over_source_builds(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            for name in NAMES:
                binary(project, 'target/release', name, 'release')
            packaged = binary(project, 'bin', 'mygdrive-mount', 'packaged')
            self.assertEqual(LAUNCHER.mount_binary(project), packaged)

    def test_snapshot_mode_does_not_read_profile_or_start_mount_or_install_icons(self):
        for arguments in [['--snapshot', 'preview.png'], ['--snapshot=preview.png']]:
            with self.subTest(arguments=arguments), \
                    mock.patch.object(LAUNCHER.Path, 'exists', side_effect=AssertionError('profile read')), \
                    mock.patch.object(LAUNCHER.subprocess, 'run') as install, \
                    mock.patch.object(LAUNCHER.subprocess, 'Popen') as start:
                self.assertIsNone(LAUNCHER.start(arguments))
                install.assert_not_called()
                start.assert_not_called()


if __name__ == '__main__':
    unittest.main()
