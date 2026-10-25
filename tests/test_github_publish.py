"""Publication integration tests use local bare Git repositories, without GitHub."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


PROJECT = Path(__file__).resolve().parents[1]


class PublishTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='mygdrive publish ')
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.source = self.base / 'exported sources'
        self.source.mkdir()
        self.remote = self.base / 'remote.git'
        self.cache = self.base / 'publisher cache'
        self.env = {**os.environ, 'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_CONFIG_NOSYSTEM': '1',
                    'GIT_AUTHOR_NAME': 'Publish Test', 'GIT_AUTHOR_EMAIL': 'test@example.invalid',
                    'GIT_COMMITTER_NAME': 'Publish Test', 'GIT_COMMITTER_EMAIL': 'test@example.invalid'}
        for key in ('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_COMMON_DIR'):
            self.env.pop(key, None)
        self.git(self.base, 'init', '--bare', '--initial-branch=main', str(self.remote))
        seed = self.base / 'seed'
        seed.mkdir()
        self.git(seed, 'init', '--initial-branch=main')
        (seed / 'remote-only.md').write_text('Keep this upstream document.\n')
        self.git(seed, 'add', '.')
        self.git(seed, 'commit', '-m', 'seed')
        self.git(seed, 'remote', 'add', 'origin', str(self.remote))
        self.git(seed, 'push', 'origin', 'main')
        (self.source / 'scripts').mkdir()
        for name in ('push-github.py', 'push-github.sh'):
            shutil.copy2(PROJECT / 'scripts' / name, self.source / 'scripts' / name)
        shutil.copy2(PROJECT / '.gitignore', self.source / '.gitignore')
        for name, contents in [('Cargo.toml', '[package]\nname="example"\nversion="1.0.0"\n'),
                               ('Cargo.lock', '# test lock\n'), ('src/lib.rs', 'pub fn example() {}\n'),
                               ('README.md', '# Local sources\n'), ('build-desktop.sh', '#!/bin/sh\nexit 0\n')]:
            self.write(name, contents)
        (self.source / 'build-desktop.sh').chmod(0o755)

    def git(self, directory, *args):
        return subprocess.run(['git', '-C', str(directory), *args], env=self.env,
                              capture_output=True, text=True, check=True).stdout.strip()

    def write(self, name, contents):
        path = self.source / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)

    def run_script(self, *extra):
        return subprocess.run(['bash', str(self.source / 'scripts/push-github.sh'),
                               'fix: changes with spaces', '--repo', str(self.remote),
                               '--cache', str(self.cache), *extra], cwd=self.base,
                              env=self.env, capture_output=True, text=True)

    def head(self):
        return self.git(self.remote, 'rev-parse', 'refs/heads/main')

    def checkout(self):
        return next(path for path in self.cache.iterdir() if (path / '.git').is_dir())

    def assert_published(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def test_export_without_git_publishes_public_files_and_preserves_upstream_extras(self):
        for name in ['oauth-client.json', 'src/.env.local', 'assets/private.pem',
                     'target/generated.rs', 'artifacts/probe.rs', '.tools/helper.sh']:
            self.write(name, 'private or generated data\n')
        self.assert_published()
        paths = self.git(self.remote, 'ls-tree', '-r', '--name-only', 'main').splitlines()
        self.assertIn('src/lib.rs', paths)
        self.assertIn('remote-only.md', paths)
        self.assertFalse(any(name in paths for name in ['oauth-client.json', 'src/.env.local',
                         'assets/private.pem', 'target/generated.rs', 'artifacts/probe.rs', '.tools/helper.sh']))
        self.assertIn('100755', self.git(self.remote, 'ls-tree', 'main', 'build-desktop.sh'))
        self.assertFalse((self.source / '.git').exists())

    def test_preview_makes_no_commits_and_leaves_no_dirty_cached_checkout(self):
        before = self.head()
        result = self.run_script('--dry-run')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('Pratinjau selesai', result.stdout)
        self.assertEqual(self.head(), before)
        self.assertFalse(any((path / '.git').is_dir() for path in self.cache.iterdir()))
        self.assert_published()

    def test_unchanged_sources_do_not_create_an_extra_commit(self):
        self.assert_published()
        before = self.head()
        self.assert_published()
        self.assertEqual(self.head(), before)

    def test_deleting_a_previously_published_file_removes_only_that_managed_file(self):
        self.write('src/obsolete.rs', '// remove later\n')
        self.assert_published()
        (self.source / 'src/obsolete.rs').unlink()
        self.assert_published()
        paths = self.git(self.remote, 'ls-tree', '-r', '--name-only', 'main').splitlines()
        self.assertNotIn('src/obsolete.rs', paths)
        self.assertIn('remote-only.md', paths)

    def test_remote_edits_conflicting_with_source_are_preserved(self):
        self.assert_published()
        peer = self.base / 'peer'
        self.git(self.base, 'clone', str(self.remote), str(peer))
        (peer / 'README.md').write_text('# Changed on GitHub\n')
        self.git(peer, 'add', '.')
        self.git(peer, 'commit', '-m', 'remote edit')
        self.git(peer, 'push', 'origin', 'main')
        before = self.head()
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('bertabrakan', result.stderr)
        self.assertEqual(self.head(), before)
        self.assertEqual(self.git(self.remote, 'show', 'main:README.md'), '# Changed on GitHub')

    def test_failed_push_can_retry_without_recreating_the_commit(self):
        hook = self.remote / 'hooks/pre-receive'
        hook.write_text('#!/bin/sh\nexit 1\n')
        hook.chmod(0o755)
        before = self.head()
        failed = self.run_script()
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual(self.head(), before)
        pending = self.git(self.checkout(), 'rev-parse', 'HEAD')
        hook.unlink()
        self.assert_published()
        self.assertEqual(self.head(), pending)

    def test_source_symlink_is_rejected_before_any_publication(self):
        secret = self.base / 'outside-secret.txt'
        secret.write_text('private\n')
        (self.source / 'src/linked.rs').symlink_to(secret)
        before = self.head()
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Symlink', result.stderr)
        self.assertEqual(self.head(), before)

    def test_a_dirty_cached_checkout_is_not_overwritten(self):
        self.assert_published()
        checkout = self.checkout()
        (checkout / 'README.md').write_text('Manual work in cache\n')
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('belum selesai', result.stderr)
        self.assertEqual((checkout / 'README.md').read_text(), 'Manual work in cache\n')

    def test_inline_credentials_are_rejected_without_echoing_them(self):
        result = self.run_script('--repo', 'https://user:fake-secret-value@github.com/example/repo.git')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('fake-secret-value', result.stderr)
        self.assertIn('tanpa token', result.stderr)


if __name__ == '__main__':
    unittest.main()
