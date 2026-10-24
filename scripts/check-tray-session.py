#!/usr/bin/python3
"""Exercise our own application's public D-Bus tray/actions in the real session.

Uses a temporary profile without tokens, never starts synchronization or enables
user autostart. Requires python3-gi and a running StatusNotifierItem host.
"""
import hashlib
import json
import subprocess
import tempfile
import time
from pathlib import Path
from gi.repository import Gio, GLib

project = Path(__file__).resolve().parent.parent
bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)


def call(name, path, interface, method, params=None):
    reply = bus.call_sync(name, path, interface, method, params, None,
                          Gio.DBusCallFlags.NONE, 3000, None)
    return reply.unpack() if reply else ()


def registered():
    return call('org.kde.StatusNotifierWatcher', '/StatusNotifierWatcher',
                'org.freedesktop.DBus.Properties', 'Get',
                GLib.Variant('(ss)', ('org.kde.StatusNotifierWatcher',
                                     'RegisteredStatusNotifierItems')))[0]


def eventually(predicate, seconds=12):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        result = predicate()
        if result:
            return result
        time.sleep(0.1)
    raise AssertionError('Expected tray state was not confirmed')


with tempfile.TemporaryDirectory(prefix='mygdrive-tray-') as temporary:
    base = Path(temporary)
    state, root = base/'state', base/'local'
    state.mkdir(mode=0o700)
    root.mkdir()
    (state/'service-config.json').write_text(json.dumps(dict(
        version=1, account='tray-check', root=str(root), drive_folder='root',
        interval_seconds=30, cache_max_bytes=None)))
    digest = hashlib.sha256(bytes(state)).hexdigest()
    app_id = 'io.mygdrive.Profile' + digest
    app_path = '/' + app_id.replace('.', '/')
    argv = [str(project/'run-desktop.sh'), '--state', str(state)]
    process = subprocess.Popen(argv + ['--background'], stdout=subprocess.DEVNULL,
                               stderr=subprocess.PIPE)
    try:
        def find_item():
            for item in registered():
                name, _, path = item.partition('/')
                if f'-{process.pid}-' in name:
                    return name, '/' + path
            return None
        name, item_path = eventually(find_item)
        identity = call(name, item_path, 'org.freedesktop.DBus.Properties', 'Get',
                        GLib.Variant('(ss)', ('org.kde.StatusNotifierItem', 'Id')))[0]
        assert identity == 'mygdrive-' + digest
        menu = call(name, item_path, 'org.freedesktop.DBus.Properties', 'Get',
                    GLib.Variant('(ss)', ('org.kde.StatusNotifierItem', 'Menu')))[0]
        def loaded_menu():
            layout = call(name, menu, 'com.canonical.dbusmenu', 'GetLayout',
                          GLib.Variant('(iias)', (0, 1, ['label', 'enabled'])))[1]
            entries = {props.get('label'): (item_id, props)
                       for item_id, props, _ in layout[2] if 'label' in props}
            return entries if entries['Mulai service'][1].get('enabled', True) else None
        entries = eventually(loaded_menu)
        assert not entries['Jeda sinkronisasi'][1]['enabled']
        assert not entries['Sinkronkan sekarang'][1]['enabled']
        assert entries['Mulai service'][1].get('enabled', True)

        actions = call(app_id, app_path, 'org.gtk.Actions', 'List')[0]
        assert {'show', 'hide', 'quit'}.issubset(actions)
        call(app_id, app_path, 'org.gtk.Actions', 'Activate',
             GLib.Variant('(sava{sv})', ('hide', [], {})))
        time.sleep(0.3)
        assert process.poll() is None and find_item()
        # Reopening a hidden profile must activate it, without another tray/writer.
        result = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                timeout=10)
        assert result.returncode == 0, result.stderr.decode(errors='replace')
        assert process.poll() is None
        assert len([item for item in registered() if f'-{process.pid}-' in item]) == 1
        call(name, menu, 'com.canonical.dbusmenu', 'Event',
             GLib.Variant('(isvu)', (entries['Tampilkan MyGdrive Rust'][0], 'clicked',
                                     GLib.Variant('i', 0), 0)))
        call(name, menu, 'com.canonical.dbusmenu', 'Event',
             GLib.Variant('(isvu)', (entries['Keluar dan hentikan sinkronisasi'][0],
                                     'clicked', GLib.Variant('i', 0), 0)))
        assert process.wait(timeout=10) == 0
        eventually(lambda: not find_item())
        assert not (state/'state.sqlite3').exists()
        assert not (state/'control/service.sock').exists()
        evidence = dict(host_registered=True, menu_verified=True,
                        hide_keeps_tray_alive=True, single_instance=True,
                        show_and_quit=True, sync_started=False,
                        user_autostart_enabled=False)
        (project/'artifacts/tray-session-check.json').write_text(
            json.dumps(evidence, indent=2) + '\n')
        print(json.dumps(evidence))
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
