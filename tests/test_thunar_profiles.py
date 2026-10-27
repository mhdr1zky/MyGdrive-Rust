"""Isolated profile routing: never modifies the user's Thunar or Drive files."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from concurrent.futures import ThreadPoolExecutor
from unittest import mock
import xml.etree.ElementTree as ET

SCRIPTS = Path(__file__).resolve().parents[1] / 'scripts'


def module(name):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / name)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


INSTALLER = module('install-thunar-actions.py')
ACTION = module('thunar-action.py')


class ThunarProfilesTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='thunar profiles % space ')
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.config = self.base / 'config'
        self.registry = self.config / 'Thunar/mygdrive-profiles.json'

    def register(self, name):
        state = self.base / f'state-{name}'
        backing = self.base / f'files-{name}'
        mount = self.base / f'online-{name}'
        state.mkdir(exist_ok=True); backing.mkdir(exist_ok=True)
        (state / 'service-config.json').write_text(json.dumps(dict(account=name, root=str(backing))))
        (state / 'service-config.json').chmod(0o600)
        INSTALLER.install(self.config, SCRIPTS.parent, state, name, mount)
        return state, backing, mount

    def test_second_profile_preserves_first_and_mixed_selection_routes_without_shell(self):
        first, local_a, mount_a = self.register('a')
        second, local_b, mount_b = self.register('b')
        self.register('a')
        profiles = INSTALLER.read_profiles(self.registry)
        self.assertEqual(len(profiles), 2)
        self.assertEqual(self.registry.stat().st_mode & 0o777, 0o600)
        paths = [str(local_a / '$(touch NEVER); file.txt'), str(mount_b / 'folder with spaces')]
        args = argparse.Namespace(paths=paths, registry=str(self.registry), binary=None, state=None,
                                  mount=None, account='default', mode='available')
        with mock.patch.object(ACTION.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, '', '')) as run:
            ACTION.dispatch(args)
        self.assertEqual(run.call_count, 2)
        argv_a, argv_b = [call.args[0] for call in run.call_args_list]
        self.assertIn(str(first), argv_a); self.assertIn(str(second), argv_b)
        self.assertEqual(argv_a[-1], paths[0]); self.assertEqual(argv_b[-1], paths[1])
        self.assertNotIn('shell', run.call_args.kwargs)
        tree = ET.fromstring((self.config / 'Thunar/uca.xml').read_bytes())
        self.assertEqual(len(tree.findall('action')), 2)
        self.assertIn('--registry', tree.find('action/command').text)
        self.assertNotIn('belum membebaskan ruang', tree.find("action[unique-id='mygdrive-offline-v1-unpin']/description").text)

    def test_legacy_fixed_profile_is_migrated_when_registering_another_account(self):
        first, root, mount = self.register('a')
        self.registry.unlink()
        import shlex
        old = ['python3', str(SCRIPTS / 'thunar-action.py'), '--binary', '/example/bin/mygdrive-sync',
               '--state', str(first), '--account', 'a', '--mount', str(mount), '--mode', 'available', '--']
        actions = ET.Element('actions'); action = ET.SubElement(actions, 'action')
        ET.SubElement(action, 'unique-id').text = INSTALLER.PREFIX + 'available'
        ET.SubElement(action, 'command').text = ' '.join(shlex.quote(part.replace('%', '%%')) for part in old) + ' %F'
        (self.config / 'Thunar/uca.xml').write_bytes(ET.tostring(actions))
        self.register('b')
        self.assertEqual({entry['account'] for entry in INSTALLER.read_profiles(self.registry)}, {'a', 'b'})

    def test_outside_ambiguous_and_traversal_selection_make_no_changes(self):
        _, local_a, mount_a = self.register('a')
        for paths in [[str(local_a / 'good'), str(self.base / 'outside')], [str(local_a / '../outside')]]:
            args = argparse.Namespace(paths=paths, registry=str(self.registry), binary=None, state=None,
                                      mount=None, account='default', mode='unpin')
            with mock.patch.object(ACTION.subprocess, 'run') as run:
                with self.assertRaises(ValueError): ACTION.dispatch(args)
                run.assert_not_called()
        profiles = INSTALLER.read_profiles(self.registry)
        profiles.append({**profiles[0], 'state': str(self.base / 'another-state')})
        with self.assertRaises(ValueError): INSTALLER.select_profiles(profiles, [str(mount_a / 'file')])

    def test_registry_symlinks_and_writable_metadata_are_rejected(self):
        self.register('a')
        original = self.registry.read_bytes()
        target = self.base / 'do-not-touch'; target.write_bytes(original)
        self.registry.unlink(); self.registry.symlink_to(target)
        with self.assertRaises(OSError): INSTALLER.read_profiles(self.registry)
        with self.assertRaises(OSError): self.register('b')
        self.assertEqual(target.read_bytes(), original)
        self.registry.unlink(); self.registry.write_bytes(original); self.registry.chmod(0o666)
        with self.assertRaises(ValueError): INSTALLER.read_profiles(self.registry)

    def test_concurrent_profile_installations_do_not_lose_accounts(self):
        with ThreadPoolExecutor(max_workers=4) as workers:
            list(workers.map(self.register, ['a', 'b', 'c', 'd']))
        self.assertEqual({entry['account'] for entry in INSTALLER.read_profiles(self.registry)}, {'a', 'b', 'c', 'd'})


if __name__ == '__main__':
    unittest.main()
