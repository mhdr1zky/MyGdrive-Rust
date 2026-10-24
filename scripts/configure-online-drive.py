#!/usr/bin/env python3
"""Persist an opt-in virtual mount, without changing the cloud sync scope."""
import argparse
import json
import os
from pathlib import Path
import tempfile


def configure(state, account, mount):
    state = state.resolve(strict=True)
    config = json.loads((state / 'service-config.json').read_text())
    if config['account'] != account:
        raise ValueError('account differs from saved profile')
    backing = Path(config['root']).resolve(strict=True)
    if mount.is_symlink():
        raise ValueError('mount must not be a symlink')
    candidate=mount.resolve(strict=False)
    if any(a == b or a in b.parents or b in a.parents for a, b in [(candidate, backing), (candidate, state)]):
        raise ValueError('mount must be separate from backing and state')
    mount.mkdir(mode=0o700, parents=True, exist_ok=True)
    mount = mount.resolve(strict=True)
    if any(a == b or a in b.parents or b in a.parents for a, b in [(mount, backing), (mount, state)]):
        raise ValueError('mount must be separate from backing and state')
    if os.path.ismount(mount) or any(mount.iterdir()):
        raise ValueError('mount must be empty and not already mounted')
    if mount.stat().st_uid != os.getuid():
        raise ValueError('mount belongs to another user')
    target = state / 'virtual-drive.json'
    if target.is_symlink():
        raise ValueError('unsafe mount configuration')
    if target.exists() and json.loads(target.read_text()).get('managed_by') != 'mygdrive':
        raise ValueError('unmanaged mount configuration exists')
    data = {'version': 1, 'managed_by': 'mygdrive', 'account': account, 'mount': str(mount)}
    fd, temp = tempfile.mkstemp(prefix='.virtual-drive-', dir=state)
    try:
        with os.fdopen(fd, 'w') as out:
            json.dump(data, out, indent=2)
            out.write('\n')
            out.flush()
            os.fsync(out.fileno())
        os.replace(temp, target)
    finally:
        if os.path.exists(temp):
            os.unlink(temp)
    return data


if __name__ == '__main__':
    p = argparse.ArgumentParser()
    p.add_argument('--state', type=Path, required=True)
    p.add_argument('--account', default='default')
    p.add_argument('--mount', type=Path, required=True)
    a = p.parse_args()
    if not a.mount.is_absolute():
        p.error('mount must be absolute')
    print(json.dumps(configure(a.state, a.account, a.mount), indent=2))
