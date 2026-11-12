# Xfce tray and start at login

MyGDrive exposes a StatusNotifierItem using [ksni](https://docs.rs/ksni/0.3.7/ksni/). Each account collection has one application instance and tray icon. Reopening a profile presents the existing window; account services remain independent.

## Usage

- Left-clicking the tray icon opens **Sync activity** at the right edge of the clicked monitor on X11/Xfce, respecting panel work areas. A bottom-panel click aligns it near the bottom; a top-panel click aligns it near the top. Wayland retains compositor-controlled placement. The panel shows with file queues, operation stages, retries, and recent results. New work sends notifications; **View details** opens the panel. **Show MyGdrive Rust** opens the dashboard. Other actions open the local folder, pause/resume, or sync now.
- Closing the window or pressing **Ctrl+W** hides it while the tray is registered; the service continues.
- **Quit**, **Ctrl+Q**, or **Quit and stop sync** requests shutdown for the running profiles in the collection, waits for active work and writer-lock release, then closes the application.
- Shutdown and restart wait for active work and writer-lock release, including scans/transfers longer than two minutes. Retrying while the service is already stopping resumes waiting.
- A foreground launch opens the dashboard. Losing the tray host reveals that foreground dashboard again. A background/autostart launch stays hidden while the host starts or reconnects; failed tray registration retries. Open the application from its launcher or choose **Show MyGdrive Rust** in the tray menu to show the dashboard. Closing the window without a tray closes only the UI and leaves the service running.

State comes from dashboard IPC. Historical snapshots do not enable transfer controls. Tray callbacks only enqueue intents. Blue circular arrows indicate active/pending file work; routine metadata checks do not activate them. A 2.5-second recent-activity interval makes fast transfers visible. The arrows are static. Red indicates confirmed service error or required Google sign-in. Yellow indicates pause, offline connectivity, Drive throttling, or a missing UI/service connection. Tooltip and activity health text explain the same condition; completed operations are history, not proof of overall health.

## Per-profile autostart

Enable **Start at login** under **Settings** on a configured dashboard. It is off by default and applies to the next Linux login; use **Start service** for the current session. Uncheck it to disable.

The backend writes a private per-profile entry under `$XDG_CONFIG_HOME/autostart`, normally `~/.config/autostart`, following [XDG Autostart](https://specifications.freedesktop.org/autostart-spec/latest/). It uses absolute launcher/state paths and `--background --start-service`. Keep source checkouts/binaries or the AppImage at their saved locations.

Arguments do not pass through a shell. `/usr/bin/env --` avoids premature GIO executable checks before percent expansion. Tests use `gio launch` with spaces, quotes, dollar signs, backticks, backslashes, and percent signs. Control characters and `=` in launcher paths are rejected. The backend changes only matching MyGDrive/profile-marked entries, rejects symlinks, and uses a lock plus atomic writes; unrelated entries are preserved.

Startup restores saved settings and pause state, avoiding a second writer when the service is already live. Autostart never opens the dashboard just because the tray host is slow or temporarily unavailable. The icon registers when the panel becomes available. There is no timeout that opens the dashboard, and a late tray registration does not hide a dashboard the user opened explicitly. Existing managed entries already contain the background argument and do not need to be re-enabled after upgrading. Autostart does not trigger OAuth login automatically; required login appears in status.

## Verification and limits

Historical tray validation covered opt-in/idempotency, profile isolation, foreign entries/symlinks, GIO quoting, quit during transfer, and missing-socket confirmation failures. The Xfce host registered an isolated tokenless profile. D-Bus checks exercised menus, disabled controls, Show, Quit, hiding, and single-instance reopening. Evidence: `scripts/check-tray-session.py`, `artifacts/tray-session-check.json`, and `tray-autostart-test-result.json`.

`python3 scripts/check-background-startup.py` uses an empty temporary profile, a private D-Bus bus, and an invisible Xephyr display. It verifies the dashboard stays hidden beyond the old 11-second timeout without a tray host, the tray registers when a temporary host appears, disconnecting the host does not reveal the dashboard, manual foreground activation opens it, and reconnecting the host or sending another background activation preserves that user-opened window. No real account, user autostart entry or service is changed.

At that stage, 61 default and 63 all-feature tests passed with formatting/Clippy clean. These are historical counts. `--snapshot` does not register tray, change autostart, or start a service even with `--start-service`. Preview: `artifacts/dashboard-preview.png`.

Real logout/login, reboot, and a live panel restart were not exercised in those tests. Temporary autostart directories were used; user autostart was not enabled by testing.

The original next step was recursive offline selections; these and online-only defaults are now implemented. See OFFLINE_MODE.md for current behavior, THUNAR_ONLINE.md for virtual-drive integration, and the changelog for later safe dehydration support.
