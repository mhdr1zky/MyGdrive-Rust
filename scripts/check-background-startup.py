#!/usr/bin/env python3
"""Check background login on an invisible Xephyr display and private D-Bus bus.

Run with: python3 scripts/check-background-startup.py
Uses an empty temporary profile; no real account, service or autostart is changed.
"""
import argparse
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

PROJECT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('previews', PROJECT / 'scripts/capture-previews.py')
PREVIEWS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PREVIEWS)


class WindowAttributes(ctypes.Structure):
    _fields_ = [(name, kind) for name, kind in [
        ('x', ctypes.c_int), ('y', ctypes.c_int), ('width', ctypes.c_int),
        ('height', ctypes.c_int), ('border_width', ctypes.c_int), ('depth', ctypes.c_int),
        ('visual', ctypes.c_void_p), ('root', ctypes.c_ulong), ('class_', ctypes.c_int),
        ('bit_gravity', ctypes.c_int), ('win_gravity', ctypes.c_int),
        ('backing_store', ctypes.c_int), ('backing_planes', ctypes.c_ulong),
        ('backing_pixel', ctypes.c_ulong), ('save_under', ctypes.c_int),
        ('colormap', ctypes.c_ulong), ('map_installed', ctypes.c_int),
        ('map_state', ctypes.c_int), ('all_event_masks', ctypes.c_long),
        ('your_event_mask', ctypes.c_long), ('do_not_propagate_mask', ctypes.c_long),
        ('override_redirect', ctypes.c_int), ('screen', ctypes.c_void_p),
    ]]


def tray_host(marker):
    from gi.repository import Gio, GLib
    xml = '''<node><interface name="org.kde.StatusNotifierWatcher">
      <method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
      <property name="ProtocolVersion" type="i" access="read"/>
      <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
      <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
      <signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
    </interface></node>'''
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    items = []

    def register(connection, sender, path, interface, method, arguments, invocation):
        item = arguments.unpack()[0]
        items.append(item)
        marker.write_text(json.dumps(items))
        invocation.return_value(None)
        connection.emit_signal(None, path, interface, 'StatusNotifierItemRegistered',
                               GLib.Variant('(s)', (item,)))

    def property_value(connection, sender, path, interface, name):
        return {'ProtocolVersion': GLib.Variant('i', 0),
                'IsStatusNotifierHostRegistered': GLib.Variant('b', True),
                'RegisteredStatusNotifierItems': GLib.Variant('as', items)}[name]

    bus.register_object('/StatusNotifierWatcher',
        Gio.DBusNodeInfo.new_for_xml(xml).interfaces[0], register, property_value, None)
    bus.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus',
        'org.freedesktop.DBus', 'RequestName',
        GLib.Variant('(su)', ('org.kde.StatusNotifierWatcher', 0)), None,
        Gio.DBusCallFlags.NONE, 3000, None)
    GLib.MainLoop().run()


def check(binary):
    lib = PREVIEWS.x11()
    lib.XGetWindowAttributes.argtypes = [ctypes.c_void_p, ctypes.c_ulong,
                                       ctypes.POINTER(WindowAttributes)]
    lib.XDestroyWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    host = lib.XOpenDisplay(os.environ.get('DISPLAY', '').encode())
    if not host:
        raise RuntimeError('A host X11 display is required for invisible Xephyr')
    parent = lib.XCreateSimpleWindow(host, lib.XDefaultRootWindow(host), 0, 0,
                                    1280, 1000, 0, 0, 0)
    lib.XSync(host, 0)  # Deliberately never map the host parent.
    nested = application = isolated = watcher = None
    with tempfile.TemporaryDirectory(prefix='mygdrive-background-') as temporary:
        directory = Path(temporary)
        env = os.environ.copy()
        for key, name in [('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'),
                          ('XDG_CACHE_HOME', 'cache'), ('XDG_RUNTIME_DIR', 'runtime')]:
            path = directory / name
            path.mkdir(mode=0o700)
            env[key] = str(path)
        state = directory / 'profile'
        state.mkdir(mode=0o700)
        env.update(GDK_BACKEND='x11', GSK_RENDERER='cairo', GTK_A11Y='none',
                   GIO_USE_VFS='local', GDK_DEBUG='no-portals', GTK_USE_PORTAL='0',
                   NO_AT_BRIDGE='1', MYGDRIVE_DISABLE_AUTO_UPDATE='1')
        with (directory / 'check.log').open('w+') as log:
            try:
                for number in range(110, 200):
                    if not Path(f'/tmp/.X{number}-lock').exists():
                        display = f':{number}'
                        break
                else:
                    raise RuntimeError('No free nested display')
                nested = subprocess.Popen(['Xephyr', display, '-parent', str(parent),
                                           '-screen', '1280x1000', '-ac', '-nolisten', 'tcp',
                                           '-no-host-grab'], stdout=log, stderr=log)
                for _ in range(100):
                    isolated = lib.XOpenDisplay(display.encode())
                    if isolated:
                        break
                    if nested.poll() is not None:
                        raise RuntimeError('Xephyr exited')
                    time.sleep(.05)
                if not isolated:
                    raise RuntimeError('Nested display unavailable')
                env['DISPLAY'] = display
                # The caller must provide a private bus, without a tray host.
                host_check = subprocess.run(['gdbus', 'call', '--session', '--dest',
                    'org.freedesktop.DBus', '--object-path', '/org/freedesktop/DBus',
                    '--method', 'org.freedesktop.DBus.NameHasOwner',
                    'org.kde.StatusNotifierWatcher'], capture_output=True, text=True, check=True)
                if 'false' not in host_check.stdout:
                    raise RuntimeError('Use dbus-run-session; a tray host already owns this bus')
                argv = [str(binary), '--state', str(state)]
                application = subprocess.Popen(argv + ['--background'], env=env,
                                               stdout=log, stderr=log)

                def visible():
                    window = PREVIEWS.find_window(lib, isolated, 'MyGdrive Rust')
                    attributes = WindowAttributes()
                    return bool(window and lib.XGetWindowAttributes(
                        isolated, window, ctypes.byref(attributes)) and attributes.map_state == 2)

                # Exceed the former 11-second fallback, checking for flashes too.
                until = time.monotonic() + 14
                while time.monotonic() < until:
                    assert application.poll() is None, 'background application exited'
                    assert not visible(), 'dashboard appeared without a tray host'
                    time.sleep(.1)
                def register_host(marker):
                    process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()),
                        '--tray-host-marker', str(marker)], env=env, stdout=log, stderr=log)
                    try:
                        until = time.monotonic() + 15
                        while not marker.exists() and time.monotonic() < until:
                            assert process.poll() is None, 'temporary tray host exited'
                            time.sleep(.1)
                        assert marker.exists(), 'tray did not register after the host appeared'
                    except BaseException:
                        if process.poll() is None:
                            process.terminate()
                        process.wait(timeout=5)
                        raise
                    return process

                watcher = register_host(directory / 'first-tray.json')
                assert not visible(), 'tray registration opened the dashboard'
                watcher.terminate()
                watcher.wait(timeout=5)
                time.sleep(1)
                assert not visible(), 'tray disconnect opened the dashboard'
                subprocess.run(argv, env=env, stdout=log, stderr=log, check=True, timeout=10)
                until = time.monotonic() + 5
                while not visible() and time.monotonic() < until:
                    time.sleep(.05)
                assert visible(), 'manual launch did not reveal the existing dashboard'
                watcher = register_host(directory / 'second-tray.json')
                time.sleep(.5)
                assert visible(), 'late tray registration hid the user-opened dashboard'
                subprocess.run(argv + ['--background'], env=env, stdout=log,
                               stderr=log, check=True, timeout=10)
                time.sleep(.5)
                assert visible(), 'background activation hid an explicitly opened dashboard'
                app_id = 'io.mygdrive.Accounts' + hashlib.sha256(bytes(state)).hexdigest()
                subprocess.run(['gapplication', 'action', app_id, 'quit'], env=env,
                               stdout=log, stderr=log, check=True, timeout=10)
                assert application.wait(timeout=10) == 0
                assert not (state / 'state.sqlite3').exists()
                print(json.dumps(dict(hidden_without_tray_for_14_seconds=True,
                    delayed_tray_registered=True, tray_disconnect_does_not_open_dashboard=True,
                    late_tray_preserves_user_window=True,
                    manual_open_works=True, background_activation_preserves_user_window=True,
                    no_real_account_or_service=True)), flush=True)
            except BaseException:
                log.flush()
                log.seek(0)
                print(log.read())
                raise
            finally:
                if isolated:
                    lib.XCloseDisplay(isolated)
                for process in [application, watcher, nested]:
                    if process is not None and process.poll() is None:
                        process.terminate()
                        process.wait(timeout=10)
                lib.XDestroyWindow(host, parent)
                lib.XCloseDisplay(host)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=PROJECT / 'target/debug/mygdrive-desktop')
    parser.add_argument('--inside-session', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--tray-host-marker', type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.tray_host_marker:
        tray_host(args.tray_host_marker)
    elif args.inside_session:
        check(args.binary.resolve())
    else:
        with tempfile.TemporaryDirectory(prefix='mygdrive-background-bus-') as temporary:
            config = Path(temporary) / 'bus.conf'
            config.write_text('<busconfig><type>session</type><listen>unix:tmpdir=/tmp</listen>'
                '<auth>EXTERNAL</auth><policy context="default"><allow send_destination="*"/>'
                '<allow receive_sender="*"/><allow own="*"/></policy></busconfig>')
            subprocess.run(['dbus-run-session', '--config-file', str(config), '--',
                sys.executable, str(Path(__file__).resolve()), '--inside-session',
                '--binary', str(args.binary.resolve())], check=True)
