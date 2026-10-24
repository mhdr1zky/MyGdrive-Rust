#!/usr/bin/env python3
"""Opt-in real FUSE/Drive CRUD and multi-file sync check, isolated under a new test folder."""
import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import threading
import time
import urllib.parse
import urllib.request
import uuid


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--state', type=Path, required=True)
    ap.add_argument('--mount', type=Path, required=True)
    ap.add_argument('--parent', default='MyGDrive-Test')
    ap.add_argument('--report', type=Path, required=True)
    ap.add_argument('--resume', action='store_true', help='Resume the initial-upload test before any rename/delete phase')
    ap.add_argument('--execute', action='store_true', help='Create and mutate only a new uniquely named test folder')
    a = ap.parse_args()
    if not a.execute:
        ap.error('--execute is required for this live test')
    if not os.path.ismount(a.mount):
        ap.error('online drive is not mounted')
    if Path(a.parent).is_absolute() or '..' in Path(a.parent).parts:
        ap.error('parent must be a relative folder inside the drive')
    if a.resume:
        result = json.loads(a.report.read_text())
        relative = result['test_folder']
        expected_prefix = Path(a.parent).as_posix() + '/Multi-Sync-Test-'
        suffix = relative.removeprefix(expected_prefix)
        if not relative.startswith(expected_prefix) or len(suffix) != 8 or any(c not in '0123456789abcdef' for c in suffix):
            ap.error('report does not identify a uniquely named test folder')
        if result.get('success') or [p['name'] for p in result['phases']] != ['initial_upload']:
            ap.error('resume is only supported after initial upload, before later successful phases')
        result.setdefault('previous_attempts', []).append({'failure': result.pop('failure', None)})
    else:
        relative = (Path(a.parent) / ('Multi-Sync-Test-' + uuid.uuid4().hex[:8])).as_posix()
        result = {'test_folder': relative, 'phases': [], 'operations': [], 'success': False}
    base = a.mount / relative
    expected = {}
    ids = {}

    def save():
        a.report.write_text(json.dumps(result, indent=2) + '\n')

    def control(action):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(8)
            client.connect(str(a.state / 'control/service.sock'))
            request = {'version': 1, 'request_id': str(uuid.uuid4()), 'session_id': None, 'action': action}
            client.sendall(json.dumps(request).encode() + b'\n')
            with client.makefile('rb') as stream:
                reply = json.loads(stream.readline(131072))
            if not reply['accepted']:
                raise RuntimeError(reply.get('error', 'service request rejected'))
            return reply['status']

    def rows():
        with sqlite3.connect('file:' + str(a.state / 'state.sqlite3') + '?mode=ro', uri=True, timeout=5) as db:
            return {json.loads(row[0])['relative_path']: json.loads(row[0])
                    for row in db.execute('SELECT data FROM files WHERE relative_path LIKE ?', (relative + '/%',))}

    def api(resource, params):
        # Authentication is read only into memory; never log or put tokens on argv.
        token_path = a.state / 'oauth' / (hashlib.sha256(b'default').hexdigest() + '.json')
        token = json.loads(token_path.read_text())['access_token']
        url = 'https://www.googleapis.com/drive/v3/' + resource + '?' + urllib.parse.urlencode(params)
        request = urllib.request.Request(url, headers={'Authorization': 'Bearer ' + token})
        with urllib.request.urlopen(request, timeout=30) as response:
            data = response.read()
        return data if params.get('alt') == 'media' else json.loads(data)

    def meta(remote_id):
        return api('files/' + urllib.parse.quote(remote_id, safe=''), {'fields': 'id,name,parents,trashed,mimeType,md5Checksum'})

    def children(remote_id):
        return api('files', {'q': "'" + remote_id.replace("'", "\\'") + "' in parents and trashed = false",
                             'fields': 'files(id,name,mimeType,md5Checksum)', 'pageSize': 100})['files']

    def operation(name, fn):
        started = time.monotonic()
        fn()
        entry = {'name': name, 'seconds': round(time.monotonic() - started, 4)}
        result['operations'].append(entry)
        save()
        print(json.dumps(entry), flush=True)

    def wait_sync(label, extra=None, timeout=300):
        started = time.monotonic()
        control('sync_now')
        last_request = started
        while time.monotonic() - started < timeout:
            current = rows()
            good = True
            for path, content in expected.items():
                record = current.get(relative + '/' + path)
                digest = hashlib.md5(content).hexdigest()
                if not record or record['status'] != 'synced' or not record.get('baseline') or record['baseline']['digest']['md5'] != digest:
                    good = False
                    break
            if good and (extra is None or extra()):
                remote_check = []
                for path, content in expected.items():
                    record = current[relative + '/' + path]
                    remote_id = record['remote_id']
                    if path in ids and ids[path] != remote_id:
                        raise AssertionError('remote identity changed: ' + path)
                    metadata = meta(remote_id)
                    body = api('files/' + remote_id, {'alt': 'media'})
                    assert not metadata['trashed'] and body == content, 'wrong server bytes: ' + path
                    ids[path] = remote_id
                    remote_check.append({'path': path, 'id': remote_id, 'md5': metadata.get('md5Checksum'), 'status': record['status']})
                phase = {'name': label, 'seconds': round(time.monotonic() - started, 3), 'verified_files': remote_check}
                result['phases'].append(phase)
                save()
                print('PHASE', json.dumps(phase), flush=True)
                return current
            if time.monotonic() - last_request > 35:
                control('sync_now')
                last_request = time.monotonic()
            time.sleep(2)
        result['last_rows'] = [{k: r.get(k) for k in ['relative_path', 'remote_id', 'status']} for r in current.values()]
        result['last_service'] = control('status')
        save()
        raise TimeoutError('sync did not converge: ' + label)

    def write(path, content):
        with (base / path).open('wb') as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())

    save()
    print('TEST FOLDER', relative, flush=True)
    try:
        if a.resume:
            for row in result['phases'][0]['verified_files']:
                ids[row['path']] = row['id']
                expected[row['path']] = ('baseline ' + row['path'] + '\n').encode()
            current = rows()
            for path, remote_id in ids.items():
                assert current[relative + '/' + path]['remote_id'] == remote_id, 'test identity changed before resume'
            base_id = result['cloud_test_folder_id']
            empty_id = result['empty_folder_id']
            nested_id = result['nested_folder_id']
        else:
            operation('create_test_folder', lambda: base.mkdir())
            operation('create_empty_folder', lambda: (base / 'empty').mkdir())
            operation('create_nested_folder', lambda: (base / 'nested').mkdir())
            for path in ['a.txt', 'b.txt', 'c.txt', 'd.txt', 'rename.txt', 'delete.txt', 'nested/child.txt']:
                expected[path] = ('baseline ' + path + '\n').encode()
                operation('create_' + path, lambda path=path: write(path, expected[path]))
            current = wait_sync('initial_upload')
            base_id = meta(current[relative + '/a.txt']['remote_id'])['parents'][0]
            initial_children = {entry['name']: entry for entry in children(base_id)}
            assert 'empty' in initial_children and 'nested' in initial_children, 'empty/new cloud directory missing'
            empty_id = initial_children['empty']['id']
            nested_id = initial_children['nested']['id']
            result['cloud_test_folder_id'] = base_id
            result['empty_folder_id'] = empty_id
            result['nested_folder_id'] = nested_id
            save()

        barrier = threading.Barrier(4)
        parallel = {path: ('parallel final ' + path + '\n').encode() for path in ['a.txt', 'b.txt', 'c.txt', 'd.txt']}
        def edit_parallel(path):
            barrier.wait()
            for revision in range(3):
                write(path, ('parallel temporary ' + str(revision) + ' ' + path).encode())
            write(path, parallel[path])
        started = time.monotonic()
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as workers:
            list(workers.map(edit_parallel, parallel))
        result['operations'].append({'name': 'four_files_edited_concurrently_with_repeated_saves', 'seconds': round(time.monotonic()-started, 4)})
        expected.update(parallel)
        wait_sync('parallel_edits')

        for revision, path in enumerate(['a.txt', 'b.txt', 'a.txt', 'c.txt', 'd.txt']):
            content = ('sequential final revision ' + str(revision) + ' ' + path + '\n').encode()
            operation('sequential_edit_' + str(revision) + '_' + path, lambda path=path,content=content: write(path, content))
            expected[path] = content
            time.sleep(.1)
        wait_sync('sequential_edits')

        renamed_id = ids.pop('rename.txt')
        delete_id = ids['delete.txt']
        child_id = ids.pop('nested/child.txt')
        operation('rename_file', lambda: (base/'rename.txt').rename(base/'renamed.txt'))
        expected['renamed.txt'] = expected.pop('rename.txt')
        ids['renamed.txt'] = renamed_id
        operation('rename_nonempty_folder', lambda: (base/'nested').rename(base/'nested-renamed'))
        expected['nested-renamed/child.txt'] = expected.pop('nested/child.txt')
        ids['nested-renamed/child.txt'] = child_id
        operation('rename_empty_folder', lambda: (base/'empty').rename(base/'empty-renamed'))
        wait_sync('file_and_folder_renames', lambda: meta(renamed_id)['name'] == 'renamed.txt' and meta(nested_id)['name'] == 'nested-renamed' and meta(empty_id)['name'] == 'empty-renamed')

        operation('delete_file', lambda: (base/'delete.txt').unlink())
        expected.pop('delete.txt');ids.pop('delete.txt')
        operation('delete_nonempty_test_folder', lambda: shutil.rmtree(base/'nested-renamed'))
        expected.pop('nested-renamed/child.txt');ids.pop('nested-renamed/child.txt')
        operation('delete_empty_folder', lambda: (base/'empty-renamed').rmdir())
        wait_sync('file_and_folder_trash', lambda: all(meta(i)['trashed'] for i in [delete_id, child_id, nested_id, empty_id]))
        remaining = children(base_id)
        assert sorted(e['name'] for e in remaining) == sorted(expected), 'unexpected duplicate/conflict or undeleted cloud item'
        result['remaining_cloud_children'] = remaining
        result['success'] = True
        result['final_service'] = control('status')
        print('SUCCESS', flush=True)
    except Exception as error:
        result['failure'] = type(error).__name__ + ': ' + str(error)
        print('FAILURE', result['failure'], flush=True)
        raise
    finally:
        save()


if __name__ == '__main__':
    main()
