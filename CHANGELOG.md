# Changelog

## v1.0.6 — 26 October 2026

- Restart the desktop automatically after a verified packaged update. Drain active synchronization first, restore the previously running service and its paused state, and preserve profile, language, and window visibility. A detached helper waits for the old instance to exit before opening the new launcher; AppImage relaunch clears old mount-specific loader settings. Failed restarts remain retryable through **Restart now**.
- Add a dynamic **Interface language** dashboard setting. Discover languages from JSON files, with **System default** always available and English, Indonesia, and Simplified Chinese bundled. **Reload languages** rescans added/removed files; optional `_language_name` metadata supplies their display names. Save the selection per user and reopen the desktop automatically without interrupting synchronization.
- Open left-click tray activity along the right edge of the clicked monitor on X11/Xfce, respecting desktop panels, monitor coordinates, and scaling. Wayland keeps compositor-controlled placement.
- Upgrading from v1.0.5 or earlier still requires one manual reopen because the old updater does not contain the new restart behavior. Existing FUSE mounts remain active to protect open files.

## v1.0.5 — 25 October 2026

- Add file-based interface localization with English, Indonesian, and Simplified Chinese catalogs. The default follows the system locale, with English fallback. Support `--language`, `MYGDRIVE_LANGUAGE`, and external JSON overrides without recompilation.
- Localize the dashboard, account setup, offline browser, activity history, tray menus, desktop notifications, and updater status.
- Include editable translation catalogs in Debian and AppImage packages.
- Maintain the README and repository Markdown documentation in English.

## v1.0.4 — 25 October 2026

- Refresh dashboard, activity, account/folder, and offline windows with consistent styling, wrapped text, and responsive buttons. Open sync activity directly from the dashboard.
- Display the running version, GitHub release status, manual checks, and release-note links.
- Periodic checks no longer install updates automatically. Package downloading and installation require **Update now**. Source builds open the release page for manual installation.

## v1.0.3 — 25 October 2026

- Introduce desktop GitHub Releases updates for amd64 `.deb` and x86_64 AppImage: six-hour checks, SHA-256 verification, atomic AppImage replacement, and `.deb` installation through Linux administrator authentication. Status appears in the desktop interface; reopen the application/service to use the new version. Source builds and snapshot mode are excluded from automatic installation.

## v1.0.2 — 25 October 2026

- A single file's metadata permission failure no longer stops other files. Store the reason per file and retry; account, connection, and throttling failures still stop the cycle.
- Mark missing old inventory entries unavailable only after a complete rebuild. Partial listings and restarts preserve the previous namespace and unuploaded local changes.
- Clean completed normal-upload outbox copies automatically. Active/cancelled operations, conflicts, review-required files, and unjournaled blobs remain protected. Completed history remains available.
- Show failure causes and recovery actions for file permissions, quota, sign-in, connectivity, throttling, and local storage.
- Add a one-command GitHub publishing script with preview, private/build-file filtering, isolated checkout, and conflict protection. Script and launcher tests run in GitHub Actions.

## v1.0.1 — 25 October 2026

- Package dashboard, folder sync, offline selections, CLI, and FUSE helper in an x86_64 AppImage. Bundle GTK; Thunar integration still needs native setup. Autostart stores the fixed AppImage path.
- Support independent file/subfolder pins, future folder children and empty directories, and explicit paused-service guidance.
- Exclude recognized Office/LibreOffice locks and Word scratch files from new queues while preserving atomic temporary-file saves.
- Complete files deleted before their first upload locally, including vanished editor locks. Files without cloud identity no longer wait for cloud deletion. Protect active journals/recovery files and recover lost create responses before issuing cloud deletes.
- Allow up to four routine clean-file metadata GETs concurrently. Process ready results first while background content transfers and mutations remain sequential. Prioritize local changes and reread changed snapshots. A 24-file mock with 40 ms latency measured 1,150 → 360 ms with unchanged GET counts and no repeated downloads; this is not a real Google Drive benchmark.
- Default new profiles to 10-second polling. Schedule successful cycles from cycle start, with at least 500 ms after long cycles. Preserve saved intervals and full failure backoff.
- Skip per-file transactions for identical scans, index directory matching by inode/path, and avoid full remote inventory reads for empty-directory ledgers. Source launchers choose the newer complete release/debug executable set without mixing desktop/service/mount builds.
- Give offline listings a dedicated persistent read-only SQLite connection, separate from writer and control/status readers, with consistent WAL snapshots.
- Open activity through notification **View details** or manual tray/menu actions instead of automatically showing it for new work.
- Unpin queues safe local-copy and related-cache cleanup after a successful online, unpaused cycle. Protect pending changes, review-required files, open handles, and remaining pins. Keep cloud files and mount metadata; reopening downloads content again.

## v1.0.0 — 24 October 2026

First Linux release with CLI, GTK4 dashboard, tray, and optional FUSE mount. The `.deb` contains `mygdrive-sync`, `mygdrive-desktop`, and `mygdrive-mount`.

### Features

- Two-way Drive sync, local watcher, cloud polling, SQLite WAL, persistent queues, and restart recovery.
- Online-only storage by default, FUSE on-demand downloads, and recursive offline file/folder selections.
- Preserve-both conflicts; recovery of writes through displaced editor inodes requires review before upload.
- Server-version checks for upload, rename, move, and Trash; concurrent changes wait for reconciliation or review.
- Browser OAuth with PKCE, separate account/folder profiles, and Unix socket pause/resume, sync-now, and drain/shutdown.
- Activity dashboard, tray, per-profile autostart, Thunar integration, verified content cache, and protective LRU cleanup.

### Release limits

My Drive and ordinary files only; no native Google Workspace content sync or Shared Drives. One pair per state, without a multi-profile supervisor. At this release, local copies could not be dehydrated automatically. Interrupted downloads restart from the beginning, and OAuth tokens are not OS-keyring encrypted. Packages target Ubuntu 24.04 amd64 with GTK4/FUSE3. See README and OAUTH_SETUP.md.
