# Performance and measurement limits

Version 1.0.1 reduces sync-check work and waiting:

- New profiles use a 10-second cloud interval; saved settings are retained.
- Successful cadence is measured from cycle start. Long cycles finish before at least 500 ms of rest; failures retain post-completion backoff.
- The watcher coalesces local changes for approximately 500 ms, then wakes the scheduler without waiting for polling. Active transfers still drain.
- Up to four routine metadata GETs overlap. Ready results are processed first and snapshots are revalidated after obtaining the file lock. Cloud/filesystem mutations and background content transfers remain sequential.
- Identical scans skip per-file transactions; directory matching uses inode/path indexes; empty-directory ledgers avoid full inventories.
- Offline listings use a dedicated read-only connection, separate from writer/controls, with consistent WAL snapshots.
- Files deleted before first upload complete locally without cloud deletion. Active journals and review protection remain enforced.

## Controlled measurement

A mock fixture used 24 downloaded files and artificial 40 ms latency per non-root metadata GET, under an unoptimized test profile.

| Metric | Before | After |
| --- | ---: | ---: |
| Cycle time | 1,150 ms | 360 ms |
| Maximum concurrent file metadata GETs | 1 | 4 |
| Total GETs including root | 25 | 25 |
| Repeated content downloads | 0 | 0 |

This fixture was about 3.2× faster. It is not a real Google Drive benchmark or internet-throughput guarantee. Metadata concurrency does not mean four background content transfers.

## Limits and validation

Initial inventory, namespace projection, and offline graphs still consume CPU/RAM on large accounts. Cloud discovery uses polling. There is no active-transfer preemption, Range download resume, or promise that sync completes within ten seconds. File sizes, network, API behavior, and conflicts affect duration.

Regressions cover local-change priority, snapshots changing during prefetch, access loss, concurrency bounds, lost create responses, restart, unpin with open files, crash recovery, and independent subfolder pins. Real FUSE tests require `/dev/fuse`. Live OAuth tests remain opt-in so automated tests do not change user Drive data.
