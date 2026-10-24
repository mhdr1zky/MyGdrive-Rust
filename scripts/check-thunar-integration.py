#!/usr/bin/env python3
"""Isolated installer checks; never touch the user's configuration."""
import importlib.util
import json
from pathlib import Path
import tempfile
import xml.etree.ElementTree as ET

base=Path(__file__).resolve().parent

def module(filename):
    spec=importlib.util.spec_from_file_location(filename,base/filename)
    obj=importlib.util.module_from_spec(spec)
    spec.loader.exec_module(obj)
    return obj

installer=module('install-thunar-actions.py')
configure=module('configure-online-drive.py')
with tempfile.TemporaryDirectory() as temp:
    root=Path(temp)
    config=root/'config'
    (config/'Thunar').mkdir(parents=True)
    uca=config/'Thunar/uca.xml'
    previous=b'<actions><!--preserve--><action><unique-id>other</unique-id><name>Other action</name><command>echo untouched</command></action></actions>'
    uca.write_bytes(previous)
    target,backup=installer.install(config,root/'project space',root/'state','default',root/'online')
    assert backup.read_bytes()==previous
    installer.install(config,root/'project space',root/'state','default',root/'online')
    tree=ET.fromstring(uca.read_bytes())
    assert len(tree.findall('action'))==3
    assert tree.find("action[unique-id='other']/command").text=='echo untouched'
    assert tree.find("action[unique-id='mygdrive-offline-v1-available']/command").text.endswith(' --mode available -- %F')
    before=uca.read_bytes();uca.unlink();uca.symlink_to(backup)
    try:
        installer.install(config,root,root/'state','default',root/'online')
        raise AssertionError('symlink accepted')
    except ValueError:
        pass
    assert backup.read_bytes()==previous
    state=root/'state';state.mkdir()
    backing=root/'backing';backing.mkdir()
    (state/'service-config.json').write_text(json.dumps({'account':'default','root':str(backing)}))
    mount=root/'online'
    saved=configure.configure(state,'default',mount)
    assert saved['mount']==str(mount)
    assert (state/'virtual-drive.json').stat().st_mode&0o777==0o600
    for bad in [backing,backing/'nested',state/'nested']:
        try:
            configure.configure(state,'default',bad)
            raise AssertionError('overlap accepted')
        except ValueError:
            pass
print('Thunar installation: preservation, backup, idempotency, argv, symlinks, private mount config and overlap checks passed')
