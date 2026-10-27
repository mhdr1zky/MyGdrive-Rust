#!/usr/bin/env python3
"""Render genuine GTK/Thunar previews with synthetic data on an invisible Xephyr display.

Requires built desktop/mount binaries, Python Pillow, Xephyr, xwd, Thunar and FUSE.
No OAuth token, Google request, existing profile or personal desktop capture is used.
Run: python3 scripts/capture-previews.py [--binary-dir target/debug]
"""
import argparse
import ctypes
import ctypes.util
import hashlib
import json
import os
from pathlib import Path
import shutil
import socketserver
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid


REPO = Path(__file__).resolve().parents[1]
OUT = REPO / "docs/screenshots"


def dump(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


class Demo:
    def __init__(self, directory, state=None):
        self.state = state or directory / "profile"
        self.root = directory / "My Drive Demo"
        self.mount = directory / "My Drive - Demo"
        for path in [self.state / "control", self.root, self.mount]:
            path.mkdir(parents=True)
        self.config = dict(version=1, account="demo", root=str(self.root),
                           drive_folder="root", interval_seconds=10, cache_max_bytes=None)
        (self.state / "service-config.json").write_text(dump(self.config))
        self.db = sqlite3.connect(self.state / "state.sqlite3", check_same_thread=False)
        for path in sorted((REPO / "migrations").glob("*.sql")):
            self.db.executescript(path.read_text())
        self.db.executescript("CREATE TABLE materialized_directories(id TEXT PRIMARY KEY,relative_path TEXT NOT NULL,remote_id TEXT UNIQUE,data TEXT NOT NULL);")
        self.files, self.remotes, self.paths = {}, {}, {}
        self.activity = False
        self.paused = False
        self.folder("documents", "Dokumen", "root")
        self.folder("photos", "Foto dan aset", "root")
        self.folder("project", "Arsip tim", "root")
        self.file("budget", "Anggaran 2026.xlsx", "root", 48256)
        self.file("guide", "Panduan tim.pdf", "root", 126976)
        self.file("notes", "Catatan rapat.txt", "root", 2048, True)
        self.file("presentation", "Presentasi produk.pptx", "root", 1572864)
        self.file("plan", "Rencana kerja.docx", "documents", 25600)
        self.file("image", "Identitas visual.png", "photos", 524288)
        self.file("brief", "Brief proyek.txt", "project", 4096, True)
        self.file("spec", "Spesifikasi produk.pdf", "project", 98304, True)
        settings = dict(account="demo", root=str(self.root), folder="root", resolved_folder="root",
                        storage_mode="online_only", offline_verified_ms=str(int(time.time()*1000)))
        meta = self.root.stat()
        settings["root_identity"] = f"{meta.st_dev}:{meta.st_ino}"
        self.db.executemany("INSERT OR REPLACE INTO settings(key,value) VALUES(?,?)", settings.items())
        self.db.executemany("INSERT INTO offline_selections(target,pinned) VALUES(?,1)", [("r:project",), ("r:notes",)])
        self.db.execute("INSERT INTO sync_cursors(name,token) VALUES('inventory','complete')")
        self.db.execute("INSERT INTO sync_cursors(name,token) VALUES('changes','demo')")
        for number, ident in enumerate(["notes", "brief", "spec"]):
            self.operation(ident, "download", "done", number)
        self.db.commit()

    def folder(self, ident, name, parent):
        self.paths[ident] = f"{self.paths[parent]}/{name}" if parent != "root" else name
        self.remote(ident, name, parent, "application/vnd.google-apps.folder", None, None)

    def remote(self, ident, name, parent, mime, size, checksum):
        remote = dict(id=ident, name=name, mimeType=mime, version="1", parents=[parent],
                      size=str(size) if size else None, sha256Checksum=checksum, md5Checksum=None,
                      headRevisionId=None, trashed=False, unavailable=False,
                      capabilities=dict(canDownload=True, canEdit=True, canTrash=True))
        self.remotes[ident] = remote
        self.db.execute("INSERT INTO remote_observations(id,data,pending) VALUES(?,?,0)", (ident, dump(remote)))
        return remote

    def file(self, ident, name, parent, size, local=False):
        path = f"{self.paths[parent]}/{name}" if parent != "root" else name
        self.paths[ident] = path
        content = ("Data contoh untuk pratinjau MyGdrive Rust.\n" * (size // 40 + 1)).encode()[:size]
        digest = dict(sha256=hashlib.sha256(content).hexdigest(), md5=hashlib.md5(content).hexdigest(), size=len(content))
        remote = self.remote(ident, name, parent, "application/octet-stream", len(content), digest["sha256"])
        fp = None
        if local:
            target = self.root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(content)
            meta = target.stat()
            fp = dict(digest=digest, device=meta.st_dev, inode=meta.st_ino,
                      modified_ns=meta.st_mtime_ns, changed_ns=meta.st_ctime_ns)
        file = dict(id=f"local-{ident}", relative_path=path, remote_id=ident, generation=0,
                    fingerprint=fp, baseline=dict(digest=digest, relative_path=path, remote=remote, remote_path=path) if local else None,
                    status="synced" if local else "online_only", materialized=local,
                    review_required=False, available_offline=False)
        self.files[ident] = file
        self.db.execute("INSERT INTO files(id,account_id,relative_path,remote_id,generation,sync_status,data) VALUES(?,?,?,?,0,?,?)",
                        (file["id"], "demo", path, ident, file["status"], dump(file)))

    def operation(self, ident, kind, stage, number):
        file = self.files[ident]
        op = dict(id=f"demo-operation-{number}", key=f"demo-key-{number}", file_id=file["id"],
                  generation=0, kind=kind, stage=stage, remote=self.remotes[ident], snapshot=None,
                  downloaded=None, target_remote_id=None, conflict_path=None, conflict_file_id=None,
                  install=None, projected_path=file["relative_path"], move_journal=None,
                  local_path=file["relative_path"], attempts=0, next_retry_ms=0, last_error=None)
        self.db.execute("INSERT INTO sync_operations(id,operation_key,file_id,stage,data) VALUES(?,?,?,?,?)",
                        (op["id"], op["key"], op["file_id"], stage, dump(op)))

    def status(self):
        now = int(time.time()*1000)
        return dict(version=1, session_id="synthetic-demo", pid=os.getpid(), running=True,
                    started_at_ms=now-600000, heartbeat_at_ms=now, stopped_at_ms=None, config=self.config,
                    phase="syncing" if self.activity else "idle", connection="online", paused=self.paused,
                    cycle_active=self.activity, sync_requested=False, shutdown_requested=False,
                    local_watcher_active=True, last_sync_started_ms=now-1000, last_sync_finished_ms=now-5000,
                    last_success_ms=now-5000, cycles_finished=24, pending_operations=1 if self.activity else 0,
                    pending_files=1 if self.activity else 0,
                    progress=dict(stage="reconciling", inventory_pages=1, inventory_items=len(self.remotes),
                                  active_operations=1 if self.activity else 0, last_activity_ms=now if self.activity else 0),
                    last_report=dict(completed=3, conflicts=0, deferred=0, errors=[]), last_error=None)

    def node(self, ident):
        folder = ident == "root" or self.remotes[ident]["mimeType"].endswith(".folder")
        descendants = [key for key in self.files if ident == "root" or key == ident or self.paths[key].startswith(self.paths[ident]+"/")]
        selected = ident in ["project", "notes"]
        inherited = ident not in ["root", "project"] and self.paths[ident].startswith(self.paths["project"]+"/")
        return dict(key="root" if ident == "root" else f"r:{ident}",
                    name="My Drive · Demo" if ident == "root" else self.remotes[ident]["name"],
                    folder=folder, selected=selected, inherited=inherited, effective=selected or inherited,
                    counts=dict(total=len(descendants), ready=sum(self.files[key]["materialized"] for key in descendants),
                                unsupported=0, dirty=0, review=0))

    def page(self, parent, offset):
        ident = "root" if parent == "root" else parent.removeprefix("r:")
        children = [self.node(key) for key, remote in self.remotes.items() if remote["parents"] == [ident]]
        children.sort(key=lambda item: (not item["folder"], item["name"]))
        return dict(current=self.node(ident), parent=None if ident == "root" else "root",
                    children=children[offset:offset+32], next=None, inventory_complete=True,
                    last_verified_ms=int(time.time()*1000), local_root=str(self.root), service_paused=False)


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        request = json.loads(self.rfile.readline(16384))
        demo = self.server.demo
        allowed = request.get("action") in ["status", "offline_list"]
        reply = dict(version=1, request_id=request["request_id"], accepted=allowed, duplicate=False,
                     error=None if allowed else "Synthetic preview is read-only", status=demo.status())
        if request.get("action") == "offline_list":
            payload = request["offline"]
            reply["offline"] = demo.page(payload["parent"], payload["offset"])
        self.wfile.write((dump(reply)+"\n").encode())


def x11():
    lib = ctypes.CDLL(ctypes.util.find_library("X11"))
    lib.XOpenDisplay.argtypes = [ctypes.c_char_p]
    lib.XOpenDisplay.restype = ctypes.c_void_p
    lib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    lib.XDefaultRootWindow.restype = ctypes.c_ulong
    lib.XCreateSimpleWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                                      ctypes.c_uint, ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_ulong]
    lib.XCreateSimpleWindow.restype = ctypes.c_ulong
    lib.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
    lib.XCloseDisplay.argtypes = [ctypes.c_void_p]
    lib.XQueryTree.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong),
                              ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)), ctypes.POINTER(ctypes.c_uint)]
    lib.XFetchName.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_char_p)]
    lib.XFree.argtypes = [ctypes.c_void_p]
    lib.XMoveResizeWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int, ctypes.c_uint, ctypes.c_uint]
    return lib


def find_window(lib, display, title):
    root = lib.XDefaultRootWindow(display)
    parent, actual_root = ctypes.c_ulong(), ctypes.c_ulong()
    children, count = ctypes.POINTER(ctypes.c_ulong)(), ctypes.c_uint()
    lib.XQueryTree(display, root, ctypes.byref(actual_root), ctypes.byref(parent), ctypes.byref(children), ctypes.byref(count))
    found = None
    for index in range(count.value):
        name = ctypes.c_char_p()
        if lib.XFetchName(display, children[index], ctypes.byref(name)) and name.value:
            text = name.value.decode(errors="replace")
            lib.XFree(name)
            if title in text:
                found = children[index]
                break
    if children:
        lib.XFree(children)
    return found


def save_xwd(source, target):
    from PIL import Image
    blob = source.read_bytes()
    header = struct.unpack(">25I", blob[:100])
    header_size, version, _, _, width, height, _, byte_order, _, _, _, bits, stride, _, red, green, blue, _, _, colors, *_ = header
    if version != 7 or bits != 32 or (red, green, blue) != (0xff0000, 0xff00, 0xff):
        raise RuntimeError(f"Unsupported XWD format: {header}")
    raw = blob[header_size+colors*12:]
    Image.frombytes("RGB", (width, height), raw, "raw", "BGRX" if byte_order == 0 else "XRGB", stride, 1).save(target)


def capture(args):
    global OUT
    if args.output_dir:
        OUT = Path(args.output_dir).resolve()
    OUT.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="mygdrive-previews-") as temp:
        directory = Path(temp)
        demo = Demo(directory)
        extra_demos = []
        if args.accounts_only:
            extra_demos = [Demo(directory / "work", demo.state / "profiles/work"),
                           Demo(directory / "archive", demo.state / "profiles/archive")]
            extra_demos[0].paused = True
            for index, (account, name, email) in enumerate(zip([demo, *extra_demos],
                    ["Alya Putri", "Tim Produk", "Arsip Pribadi"],
                    ["alya@example.test", "produk@example.test", "arsip@example.test"])):
                token = account.state / "oauth/2a97516c354b68848cdbd8f54a226a0a55b21ed138e207ad6c5cbb9c00aa5aea.json"
                token.parent.mkdir(mode=0o700)
                token.write_text(dump(dict(client=dict(client_id="preview.apps.googleusercontent.com", client_secret="synthetic"),
                    access_token="synthetic", refresh_token="synthetic", expires_at_ms=9223372036854775807,
                    scope="https://www.googleapis.com/auth/drive", drive_user_id=f"synthetic-{index}",
                    identity=dict(email=email, display_name=name))))
                token.chmod(0o600)
            token = extra_demos[1].state / "oauth/2a97516c354b68848cdbd8f54a226a0a55b21ed138e207ad6c5cbb9c00aa5aea.json"
            registration = json.loads(token.read_text())
            token.with_suffix(".client.json").write_text(dump({key: registration[key] for key in ["client", "drive_user_id", "identity"]}))
            token.with_suffix(".client.json").chmod(0o600)
            token.unlink()
        env = os.environ.copy()
        for name, folder in [("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"), ("XDG_RUNTIME_DIR", "runtime")]:
            path = directory / folder
            path.mkdir(mode=0o700)
            env[name] = str(path)
        env.update(GDK_BACKEND="x11", GDK_DEBUG="no-portals", GSK_RENDERER="cairo", LIBGL_ALWAYS_SOFTWARE="1",
                   GTK_USE_PORTAL="0", GTK_A11Y="none", GIO_USE_VFS="local", NO_AT_BRIDGE="1")
        thunar_config = Path(env["XDG_CONFIG_HOME"]) / "Thunar"
        thunar_config.mkdir()
        (thunar_config / "thunarrc").write_text("[Configuration]\nLastView=ThunarDetailsView\nLastSidePane=void\nLastLocationBar=ThunarLocationButtons\nLastWindowWidth=1100\nLastWindowHeight=730\nLastShowHidden=FALSE\n")
        xfconf = Path(env["XDG_CONFIG_HOME"]) / "xfce4/xfconf/xfce-perchannel-xml"
        xfconf.mkdir(parents=True)
        (xfconf / "thunar.xml").write_text('<?xml version="1.0" encoding="UTF-8"?><channel name="thunar" version="1.0">'
                                          '<property name="last-view" type="string" value="ThunarDetailsView"/>'
                                          '<property name="last-side-pane" type="string" value="void"/>'
                                          '<property name="last-location-bar" type="string" value="ThunarLocationButtons"/>'
                                          '<property name="last-window-width" type="int" value="1100"/>'
                                          '<property name="last-window-height" type="int" value="730"/></channel>')
        for icon in (REPO / "assets/emblems").glob("*.svg"):
            dest = Path(env["XDG_DATA_HOME"]) / "icons/hicolor/scalable/emblems" / icon.name
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(icon, dest)
        server = socketserver.ThreadingUnixStreamServer(str(demo.state / "control/service.sock"), Handler)
        server.demo = demo
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        extra_servers = []
        for account in extra_demos[:1]:
            other = socketserver.ThreadingUnixStreamServer(str(account.state / "control/service.sock"), Handler)
            other.demo = account
            other.daemon_threads = True
            threading.Thread(target=other.serve_forever, daemon=True).start()
            extra_servers.append(other)
        lib = x11()
        host_display = os.environ.get("MYGDRIVE_PREVIEW_HOST_DISPLAY", os.environ.get("DISPLAY", ""))
        host = lib.XOpenDisplay(host_display.encode())
        if not host:
            raise RuntimeError("Host X11 display unavailable")
        # Never map this parent window: the entire nested desktop stays invisible.
        parent = lib.XCreateSimpleWindow(host, lib.XDefaultRootWindow(host), 0, 0, 1600, 1100, 0, 0, 0xffffff)
        lib.XSync(host, 0)
        display = f":{args.display}"
        log = (directory / "capture.log").open("w+")
        nested = subprocess.Popen(["Xephyr", display, "-parent", str(parent), "-screen", "1600x1100", "-ac", "-nolisten", "tcp", "-no-host-grab"],
                                  env=dict(os.environ, DISPLAY=host_display), stdout=log, stderr=log)
        mount = None
        preferences = None
        isolated = None
        try:
            for _ in range(50):
                isolated = lib.XOpenDisplay(display.encode())
                if isolated:
                    break
                if nested.poll() is not None:
                    raise RuntimeError("Xephyr failed")
                time.sleep(.1)
            if not isolated:
                raise RuntimeError("Nested display unavailable")
            env["DISPLAY"] = display
            preferences = subprocess.Popen(["/usr/lib/x86_64-linux-gnu/xfce4/xfconf/xfconfd"], env=env, stdout=log, stderr=log)
            binary_dir = (REPO / args.binary_dir).resolve()
            def render(filename, option, title):
                application = subprocess.Popen([str(binary_dir / "mygdrive-desktop"), "--state", str(demo.state), *option,
                                                "--snapshot", str(OUT / filename)], env=env, stdout=log, stderr=log)
                if args.compact or option == ["--offline"]:
                    for _ in range(40):
                        window = find_window(lib, isolated, title)
                        if window:
                            width, height = (480, 760) if args.compact else (820, 960)
                            # Wait for GTK to map and apply its initial default size.
                            for _ in range(3):
                                time.sleep(.2)
                                lib.XMoveResizeWindow(isolated, window, 0, 0, width, height)
                                lib.XSync(isolated, 0)
                            break
                        time.sleep(.05)
                application.wait(timeout=20)
                if application.returncode:
                    raise RuntimeError(f"Application exited with {application.returncode}")
                if not (OUT / filename).exists():
                    raise RuntimeError(f"Missing application snapshot {filename}")

            if args.ui_test_binary:
                for test in ["account_ui_tests::switching_accounts_reuses_window_and_replaces_profile_observers",
                             "settings::tests::dashboard_discovers_new_json_without_restart_and_saves_its_selection"]:
                    subprocess.run([str(Path(args.ui_test_binary).resolve()), "--ignored", "--exact", test, "--nocapture"],
                                   env=env, check=True, timeout=60)
            if args.ipc_check:
                if not extra_demos:
                    raise RuntimeError("--ipc-check requires --accounts-only synthetic profiles")
                ipc_env = dict(env, MYGDRIVE_DISABLE_AUTO_UPDATE="1")
                primary = subprocess.Popen([str(binary_dir / "mygdrive-desktop"), "--state", str(demo.state)],
                                           env=ipc_env, stdout=log, stderr=log)
                try:
                    for _ in range(100):
                        if find_window(lib, isolated, "MyGdrive Rust"):
                            break
                        if primary.poll() is not None:
                            raise RuntimeError("Primary desktop exited before IPC test")
                        time.sleep(.1)
                    else:
                        raise RuntimeError("Primary desktop window did not appear")
                    for options in [["--background"], ["--accounts"]]:
                        forwarded = subprocess.run([str(binary_dir / "mygdrive-desktop"), "--state", str(extra_demos[0].state), *options],
                                                   env=ipc_env, stdout=log, stderr=log, timeout=8)
                        if forwarded.returncode or primary.poll() is not None:
                            raise RuntimeError("Profile launch did not forward to the existing desktop")
                    for _ in range(100):
                        if find_window(lib, isolated, "Manage accounts") or find_window(lib, isolated, "Kelola akun"):
                            break
                        time.sleep(.1)
                    else:
                        raise RuntimeError("Forwarded account manager did not open")
                    print("Single-instance profile forwarding passed")
                finally:
                    primary.terminate()
                    primary.wait(timeout=10)
            if args.accounts_only:
                render("multi-account-dashboard.png", [], "MyGdrive Rust")
                render("multi-account-manager.png", ["--accounts"], "Kelola akun")
                print("Multi-account previews captured:", OUT)
                return
            if args.maintenance_only:
                # Synthetic review/error rows; never approve or contact Drive.
                demo.file("recovery-demo", "Rencana kerja - Recovery.docx", "root", 8192, True)
                recovery = demo.files["recovery-demo"]
                recovery.update(status="recovery_review", review_required=True, remote_id=None, baseline=None)
                demo.db.execute("UPDATE files SET remote_id=NULL,sync_status='recovery_review',data=? WHERE id=?", (dump(recovery), recovery["id"]))
                problem = demo.files["budget"]
                problem["metadata_issue"] = "quota"
                demo.db.execute("UPDATE files SET data=? WHERE id=?", (dump(problem), problem["id"]))
                for folder in ["cache", "outbox", "recovery"]:
                    (demo.state / folder).mkdir(exist_ok=True)
                    (demo.state / folder / "preview.blob").write_bytes(b"preview" * 4096)
                (demo.root / ".mygdrive-internal-demo.swap").write_bytes(b"recovery" * 4096)
                demo.db.commit()
                render("storage-recovery.png", ["--maintenance"], "Storage and recovery")
                print("Storage/recovery preview captured:", OUT)
                return
            render("dashboard.png", [], "MyGdrive Rust")
            render("offline-browser.png", ["--offline"], "Tersedia offline")
            render("account-setup.png", ["--setup"], "Akun dan folder")
            demo.activity = True
            demo.operation("notes", "upload", "snapshot_ready", 9)
            demo.db.commit()
            render("sync-activity.png", ["--progress"], "Aktivitas sinkronisasi")
            demo.activity = False
            if args.gtk_only:
                print("GTK snapshots captured:", OUT)
                return
            mount = subprocess.Popen([str(binary_dir / "mygdrive-mount"), "--state", str(demo.state), "--account", "demo", "--mount", str(demo.mount)], env=env, stdout=log, stderr=log)
            for _ in range(100):
                if os.path.ismount(demo.mount):
                    break
                if mount.poll() is not None:
                    raise RuntimeError("Demo FUSE mount failed")
                time.sleep(.1)
            if not os.path.ismount(demo.mount):
                raise RuntimeError("Demo FUSE mount timed out")
            subprocess.Popen(["thunar", str(demo.mount)], env=env, stdout=log, stderr=log)
            window = None
            for _ in range(80):
                window = find_window(lib, isolated, "Demo")
                if window:
                    break
                time.sleep(.1)
            if not window:
                raise RuntimeError("Demo Thunar window unavailable")
            lib.XMoveResizeWindow(isolated, window, 0, 0, 1100, 730)
            lib.XSync(isolated, 0)
            time.sleep(3)
            raw = directory / "drive.xwd"
            subprocess.run(["xwd", "-silent", "-display", display, "-id", str(window), "-out", str(raw)], check=True, timeout=10)
            save_xwd(raw, OUT / "online-drive.png")
            print("Fresh synthetic-data previews:")
            for path in sorted(OUT.glob("*.png")):
                print(path.relative_to(REPO), path.stat().st_size)
        except Exception:
            log.flush()
            print((directory / "capture.log").read_text()[-10000:], file=sys.stderr)
            raise
        finally:
            try:
                subprocess.run(["thunar", "--quit"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=3)
            except subprocess.TimeoutExpired:
                pass
            if mount and os.path.ismount(demo.mount):
                subprocess.run(["fusermount3", "-u", str(demo.mount)], check=True, timeout=10)
            if mount:
                mount.terminate()
                mount.wait(timeout=10)
            if isolated:
                lib.XCloseDisplay(isolated)
            if preferences:
                preferences.terminate()
                preferences.wait(timeout=5)
            nested.terminate()
            nested.wait(timeout=5)
            lib.XCloseDisplay(host)
            server.shutdown()
            server.server_close()
            demo.db.close()
            for other in extra_servers:
                other.shutdown()
                other.server_close()
            for account in extra_demos:
                account.db.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary-dir", default="target/debug")
    parser.add_argument("--display", type=int, default=79)
    parser.add_argument("--compact", action="store_true", help="Verify GTK layouts at 480px window width")
    parser.add_argument("--gtk-only", action="store_true", help="Skip the native FUSE/Thunar capture")
    parser.add_argument("--accounts-only", action="store_true", help="Render three synthetic accounts: running, paused and signed out")
    parser.add_argument("--ipc-check", action="store_true", help="Verify foreground/background profile launches share one desktop")
    parser.add_argument("--ui-test-binary", help="Run the isolated GTK account switching test executable")
    parser.add_argument("--maintenance-only", action="store_true", help="Render only storage/recovery with synthetic review and quota issues")
    parser.add_argument("--output-dir", help="Use a separate output directory for package verification")
    parser.add_argument("--inside-session", action="store_true", help=argparse.SUPPRESS)
    options = parser.parse_args()
    if options.inside_session:
        capture(options)
    else:
        with tempfile.TemporaryDirectory(prefix="mygdrive-preview-session-") as session:
            session_env = os.environ.copy()
            session_env["MYGDRIVE_PREVIEW_HOST_DISPLAY"] = session_env.get("DISPLAY", "")
            session_env["DISPLAY"] = ""
            for key in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"]:
                value = Path(session) / key.lower()
                value.mkdir(mode=0o700)
                session_env[key] = str(value)
            session_env.update(GDK_DEBUG="no-portals", GTK_A11Y="none", GIO_USE_VFS="local", NO_AT_BRIDGE="1")
            bus_config = Path(session) / "bus.conf"
            bus_config.write_text('<busconfig><type>session</type><listen>unix:tmpdir=/tmp</listen><auth>EXTERNAL</auth>'
                                  '<policy context="default"><allow send_destination="*"/><allow receive_sender="*"/>'
                                  '<allow own="*"/></policy></busconfig>')
            subprocess.run(["dbus-run-session", "--config-file", str(bus_config), "--", sys.executable, str(Path(__file__).resolve()),
                            *sys.argv[1:], "--inside-session"], env=session_env, check=True)
