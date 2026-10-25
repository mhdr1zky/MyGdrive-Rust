"""Restart helper checks use temporary processes only; no desktop or updates."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / 'scripts/restart-desktop.py'
SPEC = importlib.util.spec_from_file_location('restart_desktop', SCRIPT)
RESTART = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RESTART)


class RestartTests(unittest.TestCase):
    def test_launcher_starts_only_after_old_process_exits_and_preserves_arguments(self):
        with tempfile.TemporaryDirectory(prefix='mygdrive restart ') as directory:
            marker = Path(directory) / 'launched.json'
            ready = Path(directory) / 'ready'
            payload = 'import json,sys,pathlib;pathlib.Path(sys.argv[1]).write_text(json.dumps(sys.argv[2:]))'
            parent = '''import os,subprocess,sys,time,pathlib
start=pathlib.Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()[19]
subprocess.Popen([sys.executable, sys.argv[1], '--pid', str(os.getpid()), '--start-time', start, '--', sys.executable, '-c', sys.argv[2], sys.argv[3], '--state', '/profile with $ and spaces', '--language', 'zh', '--start-service'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
pathlib.Path(sys.argv[4]).write_text('ready')
time.sleep(.5)
'''
            process = subprocess.Popen([sys.executable, '-c', parent, str(SCRIPT), payload, str(marker), str(ready)])
            try:
                deadline = time.monotonic() + 5
                while not ready.exists() and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertTrue(ready.exists())
                self.assertIsNone(process.poll())
                self.assertFalse(marker.exists(), 'new app must not activate the old instance')
                self.assertEqual(process.wait(timeout=5), 0)
                while not marker.exists() and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertEqual(json.loads(marker.read_text()), ['--state', '/profile with $ and spaces', '--language', 'zh', '--start-service'])
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=5)

    def test_process_identity_prevents_waiting_for_reused_pid(self):
        with mock.patch.object(RESTART, 'process_start', return_value='different'), \
                mock.patch.object(RESTART.select, 'select') as wait:
            RESTART.wait_for_exit(os.getpid(), 'original')
            wait.assert_not_called()

    def test_timeout_never_kills_the_old_process(self):
        with self.assertRaises(TimeoutError):
            RESTART.wait_for_exit(os.getpid(), RESTART.process_start(os.getpid()), timeout=.01)
        self.assertIsNotNone(RESTART.process_start(os.getpid()))

    def test_proc_fallback_observes_exit_without_signal_or_shell(self):
        with mock.patch.object(RESTART.os, 'pidfd_open', side_effect=OSError), \
                mock.patch.object(RESTART, 'process_start', side_effect=['old', 'old', None]), \
                mock.patch.object(RESTART.time, 'sleep') as sleep:
            RESTART.wait_for_exit(123, 'old')
            sleep.assert_called_once()

    def test_relative_launcher_is_rejected(self):
        result = subprocess.run([sys.executable, str(SCRIPT), '--pid', '1', '--start-time', '0', '--', 'relative-launcher'], capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b'absolute', result.stderr)
