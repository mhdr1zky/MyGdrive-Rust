# Online-only defaults and offline selections

MyGDrive defaults to `storage_mode=online_only`. Sync inventories Drive metadata into SQLite. Remote files never materialized locally remain `online_only` until opened through the online mount or selected offline. No empty content placeholders are created; their absence is not a local delete or a failed-sync queue entry.

## Select files and folders

1. Start the service and complete initial inventory. Open **Available offline**.
2. Enter folders by name; use **Up**, **Reload**, and **Next page**. Browsing reads the profile inventory even without network access.
3. Select **Available offline** for a file or **Make this folder available offline** for a subtree. Rules save immediately and include existing and future children.
4. Downloads wait for connectivity. If paused, choose **Resume** on the dashboard. Check local-file counts, pending changes, and unsupported formats before assuming a folder is ready.
5. Edit downloaded content in the local folder. The watcher records changes and reconciliation processes them after reconnecting.
6. **Unpin** queues cleanup after the next successful online, unpaused service cycle. Cloud files remain; reopening through the mount downloads them again.

Deleting an offline folder in the file manager removes its files and subfolders locally and queues their move to Drive Trash when synchronization resumes. Deleted materialized subfolders stay absent from the virtual namespace while that work is pending, so refreshing the view during recursive deletion does not make the parent spuriously nonempty. A saved offline pin does not recreate a deleted tree. **Unpin** remains the action for removing only clean local copies while keeping the cloud contents.

Folder rules follow stable Drive IDs through renames/moves within scope. Local file UUID rules promote to Drive IDs after upload; local-folder path rules promote when a Drive identity exists. Parent pins are inherited: unpinning a child cannot override a pinned parent. Unpin the parent if the subtree should no longer be retained.

A stopped service still permits last-inventory browsing but not selection changes. A running service can accept changes while disconnected from Google or paused. Restart older services after upgrading to migrate SQLite and enable newer actions. A never-inventoried database cannot display Drive contents.

CLI `pin FILE_ID` can select metadata-only files; it records intent, not completed availability. Run sync online afterward. State-writing CLI commands require the writer lock; use GUI/IPC while the daemon is active.

## Storage and limits

- Avoiding unselected downloads provides the main disk saving. SQLite metadata still uses space; initial engine inventory can read the whole account.
- Unpin cleanup removes supported, clean, synced local copies and related cache. Dirty, pending, review-required, open, inherited-pinned, and explicitly child-pinned files remain protected.
- Cleanup switches local state to online-only without deleting Drive files. Mount metadata remains. Upgrading alone does not discard existing local copies.
- LRU pruning removes eligible clean cache objects; effective pins, dirty/review state, and active operations protect them. Local copies and cache are separate files and can consume space twice.
- Metadata-only files appear in the GUI and optional FUSE mount. The ordinary backing folder contains only fetched content; there are no zero-byte placeholders.
- Native Google Docs/Sheets/Slides require a separate export/edit policy and are not supported offline. Download-denied files are reported; saving a rule does not mean an entire folder is ready.
- Earlier HTTP adapters restricted overwrite/Trash/rename. Current conditional mutation behavior is documented in SYNC_MUTATIONS.md; this feature alone does not imply Google Drive desktop parity.

## Implementation and durability

Schema 3 introduced `offline_selections` and durable idempotency journal `offline_commands`. Mutations carry a UUID and service session. Rules and command records commit before ACK. Retrying the same UUID does not duplicate work; mismatched payloads are rejected. Selection changes do not wait for HTTP/cycle locks. A policy gate serializes pin updates with cache eviction.

Effective selection combines node rules, inherited parents, and legacy fallback pins. `LocalFile.available_offline` alone is not the full recursive policy; UI/cache use the policy graph. Negative node rules remove an explicit/legacy pin, not an inherited parent pin.

Online-only mode skips new download plans for unmaterialized files. Unpin can cancel still-planned jobs. Repin advances generation so a cancelled job cannot block a later download. In-flight transfers may finish; cleanup waits for safety and successful sync.

Indexed recursive SQLite queries limit browser work to the subtree. Engine path projection indexes parent/name and handles duplicate-name aliases without quadratic scans. Pages contain at most 32 nodes with bounded payloads. Offline read/mutation deadlines are 30 seconds; initial frames retain 5-second/size limits.

Availability compares recorded device/inode, size, mtime, and ctime. Unscanned changes, missing files, and symlinks are not reported ready. Folder totals aggregate known children; offline metadata is the last inventory. Worker I/O and revision checks prevent old replies from replacing newer navigation/selections. SQLite observers skip unchanged inventories. With the service stopped, **Reload** rechecks local filesystem changes.

## Verification

The original offline-stage regressions covered zero-download defaults, offline selection/restart, nested/future children and rename, post-upload ID promotion, cache/dirty protection, unsupported/local readiness, IPC during transfers, session/deduplication, pagination, schema-2 upgrades, unpin/repin, indexed queries, and unchanged-inventory polling. Legacy mirror tests explicitly choose legacy mode; HTTP download tests pin the root. That historical stage recorded 72 all-feature tests.

Evidence: `artifacts/offline-browser-preview.png`, `offline-upgrade-check.json`, and `offline-mode-test-result.json`.

```bash
./build-desktop.sh --locked
./run-desktop.sh --offline
./run-desktop.sh --offline --snapshot /tmp/mygdrive-offline-browser.png
```

Snapshot mode changes no selections and starts no tray/service. Later FUSE/dehydration checks are separate; see THUNAR_ONLINE.md and the changelog.

## Thunar folder badges

An outline cloud means unpinned. Blue arrows mean the saved selection is preparing. A green check means the selected subtree is fully local and inventory is complete; inheritance applies to subfolders. Unpin restores the cloud and queues cleanup, so a cloud badge does not prove disk space has already been reclaimed. Badges do not imply native Google document offline editing support.
