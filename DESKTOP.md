# MyGDrive desktop dashboard

The compact desktop window has three tabs: **Home** shows sync health, folder shortcuts and recent files; **Sync activity** shows pending, completed and retrying files; **Settings** contains account/folder management, offline selections, storage/recovery, start-at-login, updates and language. Detailed counters, sync intervals and inventory progress remain available under **Sync details** in Settings. The header retains the account switcher and follows the native light/dark appearance.

The native GTK4/Rust `mygdrive-desktop` window observes and controls the service through private Unix IPC. The GTK thread does not open a second engine/SQLite writer or read OAuth tokens. Onboarding workers handle authentication and profile setup.

## Run

```bash
./build-desktop.sh --locked
./run-desktop.sh
./run-desktop.sh --state /path/to/state
./run-desktop.sh --language en
```

The interface follows the system language by default. The **Settings → Interface language** card always offers System default and discovers other languages from JSON catalogs (bundled: English, Indonesia, 简体中文). **Reload languages** picks up additional files without a rebuild. Applying a saved preference automatically reopens the UI while synchronization continues. See [localization](docs/LOCALIZATION.md). Documentation remains English regardless of the interface language.

The dashboard and dialogs use the active system GTK theme for light/dark surfaces, text, and accent colors. Changing the GTK theme updates open windows automatically without restarting the application.

Building from source does not replace an installed Debian package. To update the version opened from the system menu, build with `scripts/build-deb.sh`, reinstall the resulting `.deb` from `dist/`, and reopen the desktop interface.

## Dashboard and account setup

The dashboard shows live versus historical status, connection/paused state, profile and folder, pending files/operations, last success, cycle stage, session activity, and update availability. Actions include start/stop, pause/resume, sync-now, open local folder, and open in Drive. Historical snapshots do not enable live-service controls.

**Accounts and folders** supports browser login, reauthentication, paginated Drive folder browsing, and an empty local folder chooser. Each new pair gets separate state under `profiles/`; active configuration is not overwritten. Saved accounts can be reopened, including from child-profile dashboards. One desktop window manages the account collection while each profile keeps its own service. Overlap checks cover profiles in the same storage base; arbitrary CLI state elsewhere is not registered automatically.

Reauthentication uses the saved client and verifies the same Google identity without replacing the folder pair. The GUI does not print authorization URLs, codes, or tokens. Successful account login remains saved even before folder selection finishes; failed login drafts are cleaned. Older saved identities can be enriched through Drive About, checking permission identity. Invalid old configuration blocks new pair creation until reviewed.

Folder browsing uses [Drive files.list parent/MIME queries](https://developers.google.com/workspace/drive/api/guides/search-files), and IDs distinguish duplicate names. The `drive` OAuth scope still covers the account; folder selection is an application rule.

## Dependencies and local SDK

The optional Cargo feature `desktop-ui` uses [gtk4-rs](https://gtk-rs.org/gtk4-rs/stable/latest/docs/gtk4/). Core/CLI builds do not require GTK. UI work stays on the main thread; polling, filesystem, IPC, and service startup run in workers. Mutations reuse the status session ID and retry UUID described in SERVICE.md.

```bash
sudo apt-get install libgtk-4-dev
./build-desktop.sh --locked
```

The test machine provided GTK4 4.14.5. A local SDK can instead be extracted from official distro `.deb` files without installing system packages:

```bash
python3 scripts/prepare-gtk-sdk.py
./build-desktop.sh --locked
```

The build script detects `.tools/gtk-sdk`, adjusts pkg-config, and uses `.tools/cargo` when present. The launcher sets supplemental runtime library paths. `.tools` is Git-ignored and distro/architecture-specific; it is not an application distribution package.

## Application menu

```bash
python3 scripts/install-desktop.py
```

The local installer writes a user `io.mygdrive.Desktop.desktop` entry with the SVG icon and absolute project launcher. Unmanaged entries are not overwritten. Keep the checkout and binaries at their referenced locations.

## Verification and snapshots

Controller tests cover live/historical observations, resume, orderly controller close without stopping the service, startup from saved settings, pause restoration, and duplicate-writer prevention. Onboarding tests cover pagination/empty pages, folder validation, private configuration without overwrite, aliases/overlaps, concurrent saves, and corrupt settings. Historical scope-migration validation recorded 79 default and 82 all-feature tests; one live onboarding test remained opt-in. Live folder discovery succeeded without changing Drive, but fresh interactive browser consent and every GUI click were not retested end to end.

```bash
./run-desktop.sh --snapshot /tmp/mygdrive-dashboard.png
./run-desktop.sh --setup --snapshot /tmp/mygdrive-setup.png
./run-desktop.sh --offline --snapshot /tmp/mygdrive-offline-browser.png
./run-desktop.sh --language zh --snapshot /tmp/mygdrive-dashboard-zh.png
python3 scripts/capture-previews.py --gtk-only --theme Mint-Y-Dark --output-dir /tmp/mygdrive-dark-preview
```

Snapshot mode renders the actual GTK window after four seconds and exits. It does not change selections, start the service/tray, or send pause/resume/shutdown. Images can include the selected profile's paths and status. Progress is stage-based, without per-file byte percentages; one pair is controlled per state.

## Online-only and offline browsing

New profiles inventory metadata only; content downloads when opened through the online mount or selected under **Available offline**. Existing local copies are retained. The browser displays availability, pending changes, and unsupported formats. It reads the last inventory when the service is stopped; selection changes require a live service, which may be paused or disconnected from Google.

```bash
./run-desktop.sh --offline
```

FUSE on-demand access and Thunar actions are opt-in; see THUNAR_ONLINE.md and OFFLINE_MODE.md. The default test profile was expanded to all My Drive, with only MyGDrive-Test pinned. Mount: `/home/user/MyGDrive-Online`; backing test subtree: `/home/user/MyGDrive/MyGDrive-Test`. Shared Drives and the complete Shared with me namespace are excluded. Journaled migration is documented in STREAM_MY_DRIVE.md. Arbitrary GUI scope switching, prepopulated Local Folder Sync, and simultaneous multi-pair editing in one service remain future work.

## Activity and disconnections

**View details** in a desktop notification opens activity. New operations notify without automatically opening the panel; tray/menu actions also open it. Queue/deletion totals update dynamically and count waiting files once, including recorded deletes before a network operation exists. The persistent SQLite journal provides up to 30 pending and 10 completed entries; WAL reads run in workers. The activity bar has no byte percentage. Completed history is not overall service health.

Disconnects retain upload snapshots and journals. Failed work waits for backoff and is shown pending. Failed/cancelled download `.part` files are cleaned; old local content is replaced only after complete checksum/version validation. Downloads restart from the beginning; Range resume and resumable upload are unavailable. Lost create responses reconcile the same Drive ID to prevent duplicates.

## Automatic update restart

After a verified `.deb` or AppImage update, the desktop asks the service to drain, waits for the writer lock to be released, and starts a detached helper. The helper waits for the old desktop PID/start identity to exit before executing the updated launcher, avoiding single-instance activation of the old window. The same profile, language, visibility, and paused/running state are retained. Old AppImage-specific loader/schema environment entries are removed for the new image. Restart failures stay visible and can be retried; the helper writes `STATE/desktop-restart.log`. Active FUSE mounts are not forcibly detached while editors may hold them. Upgrades from v1.0.5 or earlier need one manual reopen to activate this behavior.
