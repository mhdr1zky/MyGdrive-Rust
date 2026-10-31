use async_trait::async_trait;
use bytes::Bytes;
use mygdrive_sync::{
    drive::{ByteStream, DriveClient},
    local::{fingerprint, LocalWatcher},
    model::*,
    reconcile::{reconcile, Decision},
    Error, Result, SyncEngine,
};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct FakeDrive {
    files: Mutex<HashMap<String, (RemoteFile, Vec<u8>)>>,
    next: AtomicUsize,
    conditional: bool,
    fail_after_create: AtomicBool,
    edit_during_upload: Mutex<Option<(PathBuf, Vec<u8>)>>,
    edit_during_trash: Mutex<Option<(PathBuf, Vec<u8>)>>,
    offline: AtomicBool,
    auth_required: AtomicBool,
    reject_cursor_once: AtomicBool,
    hold_get: AtomicBool,
    get_entered: tokio::sync::Notify,
    release_get: tokio::sync::Notify,
    download_calls: AtomicUsize,
    get_calls: Mutex<Vec<String>>,
    get_override: Mutex<Option<(String, usize, Option<RemoteFile>)>>,
    get_failure: Mutex<Option<(String, u16, String)>>,
    hold_get_after: Mutex<Option<(String, usize)>>,
    hold_download: AtomicBool,
    fail_download_midstream: AtomicBool,
    download_entered: tokio::sync::Notify,
    release_download: tokio::sync::Notify,
}
impl FakeDrive {
    fn new(conditional: bool) -> Arc<Self> {
        let me = Arc::new(Self {
            files: Mutex::new(HashMap::new()),
            next: AtomicUsize::new(1),
            conditional,
            fail_after_create: AtomicBool::new(false),
            edit_during_upload: Mutex::new(None),
            edit_during_trash: Mutex::new(None),
            offline: AtomicBool::new(false),
            auth_required: AtomicBool::new(false),
            reject_cursor_once: AtomicBool::new(false),
            hold_get: AtomicBool::new(false),
            get_entered: tokio::sync::Notify::new(),
            release_get: tokio::sync::Notify::new(),
            download_calls: AtomicUsize::new(0),
            get_calls: Mutex::new(Vec::new()),
            get_override: Mutex::new(None),
            get_failure: Mutex::new(None),
            hold_get_after: Mutex::new(None),
            hold_download: AtomicBool::new(false),
            fail_download_midstream: AtomicBool::new(false),
            download_entered: tokio::sync::Notify::new(),
            release_download: tokio::sync::Notify::new(),
        });
        me.files.lock().unwrap().insert(
            "root".into(),
            (
                RemoteFile {
                    id: "root".into(),
                    name: "root".into(),
                    mime_type: "application/vnd.google-apps.folder".into(),
                    version: "1".into(),
                    ..Default::default()
                },
                vec![],
            ),
        );
        me
    }
    fn put(&self, id: &str, name: &str, data: &[u8]) {
        let mut files = self.files.lock().unwrap();
        let version = files
            .get(id)
            .and_then(|(f, _)| f.version.parse::<u64>().ok())
            .unwrap_or(0)
            + 1;
        files.insert(
            id.into(),
            (
                RemoteFile {
                    id: id.into(),
                    name: name.into(),
                    mime_type: "text/plain".into(),
                    version: version.to_string(),
                    sha256_checksum: Some(format!("{:x}", Sha256::digest(data))),
                    size: Some(data.len().to_string()),
                    parents: vec!["root".into()],
                    capabilities: Capabilities {
                        can_download: Some(true),
                        can_edit: Some(true),
                        can_trash: Some(true),
                    },
                    ..Default::default()
                },
                data.to_vec(),
            ),
        );
    }
    fn check(&self) -> Result<()> {
        if self.auth_required.load(Ordering::SeqCst) {
            return Err(Error::AuthRequired);
        }
        if self.offline.load(Ordering::SeqCst) {
            Err(Error::Http {
                status: 503,
                reason: "offline mock".into(),
            })
        } else {
            Ok(())
        }
    }
    fn data(&self, id: &str) -> Vec<u8> {
        self.files.lock().unwrap().get(id).unwrap().1.clone()
    }
    fn upload_hook(&self) {
        if let Some((path, bytes)) = self.edit_during_upload.lock().unwrap().take() {
            std::fs::write(path, bytes).unwrap();
        }
    }
}
#[async_trait]
impl DriveClient for FakeDrive {
    async fn start_page_token(&self) -> Result<String> {
        self.check()?;
        Ok("start".into())
    }
    async fn list_files(&self, _: Option<&str>) -> Result<FilePage> {
        self.check()?;
        Ok(FilePage {
            files: self
                .files
                .lock()
                .unwrap()
                .values()
                .map(|(f, _)| f.clone())
                .collect(),
            next_page_token: None,
        })
    }
    async fn changes(&self, _: &str) -> Result<ChangePage> {
        self.check()?;
        if self.reject_cursor_once.swap(false, Ordering::SeqCst) {
            return Err(Error::Http {
                status: 410,
                reason: "rejected cursor".into(),
            });
        }
        Ok(ChangePage {
            changes: self
                .files
                .lock()
                .unwrap()
                .values()
                .map(|(f, _)| Change {
                    file_id: f.id.clone(),
                    removed: false,
                    file: Some(f.clone()),
                })
                .collect(),
            next_page_token: None,
            new_start_page_token: Some("next".into()),
        })
    }
    async fn get(&self, id: &str) -> Result<Option<RemoteFile>> {
        if let Some((target, status, reason)) = self.get_failure.lock().unwrap().as_ref() {
            if target == id {
                return Err(Error::Http {
                    status: *status,
                    reason: reason.clone(),
                });
            }
        }
        self.get_calls.lock().unwrap().push(id.into());
        self.check()?;
        if self.hold_get.swap(false, Ordering::SeqCst) {
            self.get_entered.notify_one();
            self.release_get.notified().await;
        }
        let targeted_hold = {
            let mut hold = self.hold_get_after.lock().unwrap();
            match hold.as_mut() {
                Some((target, remaining)) if target == id => {
                    if *remaining == 0 {
                        hold.take();
                        true
                    } else {
                        *remaining -= 1;
                        false
                    }
                }
                _ => false,
            }
        };
        if targeted_hold {
            self.get_entered.notify_one();
            self.release_get.notified().await;
        }
        let mut override_get = self.get_override.lock().unwrap();
        if let Some((target, remaining, _)) = override_get.as_mut() {
            if target == id {
                if *remaining == 0 {
                    return Ok(override_get.take().unwrap().2);
                }
                *remaining -= 1;
            }
        }
        Ok(self.files.lock().unwrap().get(id).map(|(f, _)| f.clone()))
    }
    async fn download(&self, id: &str) -> Result<ByteStream> {
        self.check()?;
        self.download_calls.fetch_add(1, Ordering::SeqCst);
        if self.hold_download.swap(false, Ordering::SeqCst) {
            self.download_entered.notify_one();
            self.release_download.notified().await;
        }
        let bytes = self.data(id);
        if self.fail_download_midstream.swap(false, Ordering::SeqCst) {
            return Ok(Box::pin(futures_util::stream::iter([
                Ok(Bytes::copy_from_slice(&bytes[..bytes.len() / 2])),
                Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "simulated disconnected download",
                ))),
            ])));
        }
        Ok(Box::pin(futures_util::stream::once(async move {
            Ok(Bytes::from(bytes))
        })))
    }
    async fn reserve_id(&self) -> Result<String> {
        self.check()?;
        Ok(format!("new-{}", self.next.fetch_add(1, Ordering::SeqCst)))
    }
    async fn create(
        &self,
        id: &str,
        name: &str,
        parent: &str,
        snapshot: &Snapshot,
    ) -> Result<RemoteFile> {
        self.check()?;
        if let Some(file) = self.get(id).await? {
            return if file.matches(&snapshot.digest) == Some(true) {
                Ok(file)
            } else {
                Err(Error::Precondition)
            };
        }
        let bytes = std::fs::read(&snapshot.path)?;
        self.upload_hook();
        self.put(id, name, &bytes);
        self.files.lock().unwrap().get_mut(id).unwrap().0.parents = vec![parent.into()];
        if self.fail_after_create.swap(false, Ordering::SeqCst) {
            return Err(Error::Http {
                status: 503,
                reason: "ambiguous committed response".into(),
            });
        }
        Ok(self.get(id).await?.unwrap())
    }
    fn conditional_mutations(&self) -> bool {
        self.conditional
    }
    async fn update(&self, remote: &RemoteFile, snapshot: &Snapshot) -> Result<RemoteFile> {
        self.check()?;
        if self.get(&remote.id).await?.unwrap().version != remote.version {
            return Err(Error::Precondition);
        }
        let bytes = std::fs::read(&snapshot.path)?;
        self.upload_hook();
        self.put(&remote.id, &remote.name, &bytes);
        Ok(self.get(&remote.id).await?.unwrap())
    }
    async fn list_children(&self, parent: &str) -> Result<Vec<RemoteFile>> {
        self.check()?;
        Ok(self
            .files
            .lock()
            .unwrap()
            .values()
            .filter(|(r, _)| !r.trashed && r.parents.iter().any(|p| p == parent))
            .map(|(r, _)| r.clone())
            .collect())
    }
    async fn trash(&self, remote: &RemoteFile) -> Result<RemoteFile> {
        self.check()?;
        let mut files = self.files.lock().unwrap();
        let (current, _) = files.get_mut(&remote.id).ok_or(Error::Precondition)?;
        if current.trashed {
            return Ok(current.clone());
        }
        if current.version != remote.version {
            return Err(Error::Precondition);
        }
        if let Some((path, bytes)) = self.edit_during_trash.lock().unwrap().take() {
            std::fs::write(path, bytes)?;
        }
        current.trashed = true;
        current.version = (current.version.parse::<u64>().unwrap() + 1).to_string();
        Ok(current.clone())
    }
    async fn rename(&self, remote: &RemoteFile, name: &str, parent: &str) -> Result<RemoteFile> {
        let mut files = self.files.lock().unwrap();
        let (current, _) = files.get_mut(&remote.id).unwrap();
        if current.version != remote.version {
            return Err(Error::Precondition);
        }
        current.name = name.into();
        current.parents = vec![parent.into()];
        current.version = (current.version.parse::<u64>().unwrap() + 1).to_string();
        Ok(current.clone())
    }
    async fn create_folder(&self, id: &str, name: &str, parent: &str) -> Result<RemoteFile> {
        if let Some(file) = self.get(id).await? {
            return Ok(file);
        }
        let file = RemoteFile {
            id: id.into(),
            name: name.into(),
            mime_type: "application/vnd.google-apps.folder".into(),
            version: "1".into(),
            parents: vec![parent.into()],
            ..Default::default()
        };
        self.files
            .lock()
            .unwrap()
            .insert(id.into(), (file.clone(), vec![]));
        Ok(file)
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    drive: Arc<FakeDrive>,
}
impl Fixture {
    fn new(conditional: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sync");
        std::fs::create_dir(&root).unwrap();
        Self {
            state: temp.path().join("state"),
            root,
            _temp: temp,
            drive: FakeDrive::new(conditional),
        }
    }
    async fn engine(&self) -> SyncEngine {
        let engine = SyncEngine::open(&self.root, &self.state, "test", "root", self.drive.clone())
            .await
            .unwrap();
        // Legacy mirror scenarios explicitly opt in; production defaults to online-only.
        sqlx::query("UPDATE settings SET value='mirror' WHERE key='storage_mode'")
            .execute(&engine.db.pool)
            .await
            .unwrap();
        engine
    }
    async fn synced(&self) -> SyncEngine {
        self.drive.put("original", "note.txt", b"baseline");
        let engine = self.engine().await;
        let report = engine.sync_once().await.unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(
            std::fs::read(self.root.join("note.txt")).unwrap(),
            b"baseline"
        );
        engine
    }
}

#[tokio::test]
async fn offline_edit_survives_restart_and_revert_clears_dirty() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    std::fs::write(fixture.root.join("note.txt"), b"offline edit").unwrap();
    fixture.drive.offline.store(true, Ordering::SeqCst);
    assert!(engine.sync_once().await.is_err());
    let file = engine.db.by_remote("original").await.unwrap().unwrap();
    assert_eq!(file.status, "modified_local");
    let generation = file.generation;
    drop(engine);
    let engine = fixture.engine().await;
    engine.scanner.scan().await.unwrap();
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .generation,
        generation
    );
    std::fs::write(fixture.root.join("note.txt"), b"baseline").unwrap();
    engine.scanner.scan().await.unwrap();
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "synced"
    );
}

#[tokio::test]
async fn watcher_persists_modified_local_without_network() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let watcher = LocalWatcher::start(&fixture.root).unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(watcher.run(engine.scanner.clone(), stop.clone()));
    std::fs::write(fixture.root.join("note.txt"), b"watched offline").unwrap();
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if engine
                .db
                .by_remote("original")
                .await
                .unwrap()
                .unwrap()
                .status
                == "modified_local"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn scanner_move_onto_metadata_only_destination_preserves_its_remote_identity() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    sqlx::query("UPDATE settings SET value='online_only' WHERE key='storage_mode'")
        .execute(&engine.db.pool)
        .await
        .unwrap();
    fixture
        .drive
        .put("destination", "destination.txt", b"cloud destination");
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    let destination = engine.db.by_remote("destination").await.unwrap().unwrap();
    assert!(!destination.materialized);
    assert!(!fixture.root.join("destination.txt").exists());
    assert_eq!(engine.db.materialized_files().await.unwrap().len(), 1);
    // Exercise the bounded path lookup across its 512-bind batch boundary.
    let mut paths: Vec<_> = (0..1024).map(|i| format!("not-present-{i}")).collect();
    paths[511] = "destination.txt".into();
    paths.push("note.txt".into());
    let occupied = engine.db.occupied_file_paths(&paths).await.unwrap();
    assert_eq!(occupied.len(), 2);
    assert!(occupied.contains("destination.txt"));
    assert!(occupied.contains("note.txt"));

    std::fs::write(fixture.root.join("note.txt"), b"moved local bytes").unwrap();
    std::fs::rename(
        fixture.root.join("note.txt"),
        fixture.root.join("destination.txt"),
    )
    .unwrap();
    engine.scanner.scan().await.unwrap();

    // Destination already belongs to another cloud file. It is an edit of that
    // identity plus a source deletion, never an inode move into an occupied row.
    let edited = engine.db.by_remote("destination").await.unwrap().unwrap();
    assert_eq!(edited.id, destination.id);
    assert_eq!(edited.relative_path, "destination.txt");
    assert!(edited.materialized);
    assert_eq!(
        edited.fingerprint.as_ref().unwrap().digest.sha256,
        format!("{:x}", Sha256::digest(b"moved local bytes"))
    );
    let source = engine.db.by_remote("original").await.unwrap().unwrap();
    assert_eq!(source.relative_path, "note.txt");
    assert!(source.materialized);
    assert!(source.fingerprint.is_none());
    assert_eq!(
        std::fs::read(fixture.root.join("destination.txt")).unwrap(),
        b"moved local bytes"
    );
    assert_eq!(fixture.drive.data("destination"), b"cloud destination");
    engine.scanner.scan().await.unwrap();
    assert_eq!(
        engine
            .db
            .by_remote("destination")
            .await
            .unwrap()
            .unwrap()
            .id,
        destination.id
    );
}

#[tokio::test]
async fn local_upload_and_remote_download_have_no_echo() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    std::fs::write(fixture.root.join("note.txt"), b"local update").unwrap();
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(fixture.drive.data("original"), b"local update");
    fixture.drive.put("original", "note.txt", b"server update");
    engine.sync_once().await.unwrap();
    assert_eq!(
        std::fs::read(fixture.root.join("note.txt")).unwrap(),
        b"server update"
    );
    engine.scanner.scan().await.unwrap();
    let file = engine.db.by_remote("original").await.unwrap().unwrap();
    assert_eq!(file.status, "synced");
    let report = engine.sync_once().await.unwrap();
    assert_eq!(report.completed, 0);
}

#[tokio::test]
async fn concurrent_edits_preserve_both_and_retry_does_not_duplicate_conflicts() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    std::fs::write(fixture.root.join("note.txt"), b"local variant").unwrap();
    fixture.drive.put("original", "note.txt", b"remote variant");
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.conflicts, 1);
    assert_eq!(
        std::fs::read(fixture.root.join("note.txt")).unwrap(),
        b"remote variant"
    );
    assert_eq!(
        std::fs::read(fixture.root.join("note (Conflict).txt")).unwrap(),
        b"local variant"
    );
    engine.sync_once().await.unwrap();
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn unverified_adapter_preserves_server_original_even_for_local_only_edit() {
    let fixture = Fixture::new(false);
    let engine = fixture.synced().await;
    std::fs::write(fixture.root.join("note.txt"), b"local variant").unwrap();
    let report = engine.sync_once().await.unwrap();
    assert_eq!(report.conflicts, 1);
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(fixture.drive.data("original"), b"baseline");
    assert_eq!(
        std::fs::read(fixture.root.join("note (Conflict).txt")).unwrap(),
        b"local variant"
    );
}

#[tokio::test]
async fn file_recreated_during_cloud_trash_is_preserved_for_review() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let path = fixture.root.join("note.txt");
    std::fs::remove_file(&path).unwrap();
    *fixture.drive.edit_during_trash.lock().unwrap() =
        Some((path.clone(), b"new local bytes during Trash".to_vec()));
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(
        fixture
            .drive
            .get("original")
            .await
            .unwrap()
            .unwrap()
            .trashed
    );
    assert_eq!(
        std::fs::read(path).unwrap(),
        b"new local bytes during Trash"
    );
    let row = engine.db.by_remote("original").await.unwrap().unwrap();
    assert_eq!(row.status, "delete_edit_review");
    assert!(row.review_required);
    engine.sync_once().await.unwrap();
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn batch_concurrent_then_sequential_edits_keep_original_ids_and_latest_bytes() {
    let fixture = Fixture::new(true);
    let count = 8;
    for index in 0..count {
        fixture.drive.put(
            &format!("batch-{index}"),
            &format!("batch-{index}.txt"),
            b"baseline",
        );
    }
    let engine = fixture.engine().await;
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");

    // Release all writers together: each file has an independent dirty generation.
    let barrier = Arc::new(std::sync::Barrier::new(count));
    std::thread::scope(|scope| {
        for index in 0..count {
            let barrier = barrier.clone();
            let path = fixture.root.join(format!("batch-{index}.txt"));
            scope.spawn(move || {
                barrier.wait();
                std::fs::write(path, format!("concurrent-{index}")).unwrap();
            });
        }
    });
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.conflicts, 0);
    for index in 0..count {
        let id = format!("batch-{index}");
        let expected = format!("concurrent-{index}").into_bytes();
        assert_eq!(fixture.drive.data(&id), expected);
        let row = engine.db.by_remote(&id).await.unwrap().unwrap();
        assert_eq!(row.status, "synced");
    }

    // Save files one after another across two passes, including several edits
    // before a sync pass. Only the final contents should be uploaded.
    for round in 1..=2 {
        for index in 0..count {
            let path = fixture.root.join(format!("batch-{index}.txt"));
            std::fs::write(&path, format!("intermediate-{round}-{index}")).unwrap();
            std::fs::write(&path, format!("sequential-{round}-{index}")).unwrap();
        }
        let report = engine.sync_once().await.unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(report.conflicts, 0);
        for index in 0..count {
            let id = format!("batch-{index}");
            let expected = format!("sequential-{round}-{index}").into_bytes();
            assert_eq!(fixture.drive.data(&id), expected);
            assert_eq!(
                std::fs::read(fixture.root.join(format!("batch-{index}.txt"))).unwrap(),
                expected
            );
            let row = engine.db.by_remote(&id).await.unwrap().unwrap();
            assert_eq!(row.status, "synced");
        }
    }
    // Root plus the original eight files; no duplicate or conflict uploads.
    assert_eq!(fixture.drive.files.lock().unwrap().len(), count + 1);
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.completed, 0);
    assert_eq!(report.conflicts, 0);
}

#[tokio::test]
async fn batch_divergent_edits_preserve_every_variant_without_retry_duplicates() {
    let fixture = Fixture::new(true);
    let count = 4;
    for index in 0..count {
        fixture.drive.put(
            &format!("divergent-{index}"),
            &format!("divergent-{index}.txt"),
            b"baseline",
        );
    }
    let engine = fixture.engine().await;
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    for index in 0..count {
        std::fs::write(
            fixture.root.join(format!("divergent-{index}.txt")),
            format!("local variant {index}"),
        )
        .unwrap();
        fixture.drive.put(
            &format!("divergent-{index}"),
            &format!("divergent-{index}.txt"),
            format!("server variant {index}").as_bytes(),
        );
    }
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.conflicts, count);
    for index in 0..count {
        let remote = format!("server variant {index}").into_bytes();
        let local = format!("local variant {index}").into_bytes();
        assert_eq!(fixture.drive.data(&format!("divergent-{index}")), remote);
        assert_eq!(
            std::fs::read(fixture.root.join(format!("divergent-{index}.txt"))).unwrap(),
            remote
        );
        assert_eq!(
            std::fs::read(
                fixture
                    .root
                    .join(format!("divergent-{index} (Conflict).txt"))
            )
            .unwrap(),
            local
        );
    }
    // The next pass also handles an implementation that queues conflict uploads.
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.conflicts, 0);
    {
        let files = fixture.drive.files.lock().unwrap();
        assert_eq!(files.len(), count * 2 + 1);
        for index in 0..count {
            let conflict_name = format!("divergent-{index} (Conflict).txt");
            let variants: Vec<_> = files
                .values()
                .filter(|(file, _)| file.name == conflict_name)
                .collect();
            assert_eq!(variants.len(), 1);
            assert_eq!(variants[0].1, format!("local variant {index}").as_bytes());
        }
    }
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.completed, 0);
    assert_eq!(fixture.drive.files.lock().unwrap().len(), count * 2 + 1);
}

#[tokio::test]
async fn ack_does_not_clear_edit_made_during_upload() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    std::fs::write(fixture.root.join("note.txt"), b"generation N").unwrap();
    *fixture.drive.edit_during_upload.lock().unwrap() =
        Some((fixture.root.join("note.txt"), b"generation N+1".to_vec()));
    engine.sync_once().await.unwrap();
    assert_eq!(fixture.drive.data("original"), b"generation N");
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "modified_local"
    );
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.conflicts, 0);
    assert_eq!(fixture.drive.data("original"), b"generation N+1");
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "synced"
    );
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 2);
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.completed, 0);
}

#[tokio::test]
async fn local_rename_and_edit_converge_on_original_remote_identity() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    std::fs::rename(
        fixture.root.join("note.txt"),
        fixture.root.join("renamed-and-edited.txt"),
    )
    .unwrap();
    std::fs::write(
        fixture.root.join("renamed-and-edited.txt"),
        b"saved after rename",
    )
    .unwrap();
    // Content and path can require separate operations; both must converge.
    for _ in 0..3 {
        let report = engine.sync_once().await.unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(report.conflicts, 0);
    }
    let remote = fixture.drive.get("original").await.unwrap().unwrap();
    assert_eq!(remote.name, "renamed-and-edited.txt");
    assert_eq!(fixture.drive.data("original"), b"saved after rename");
    assert!(!fixture.root.join("note.txt").exists());
    assert_eq!(
        std::fs::read(fixture.root.join("renamed-and-edited.txt")).unwrap(),
        b"saved after rename"
    );
    let row = engine.db.by_remote("original").await.unwrap().unwrap();
    assert_eq!(row.relative_path, "renamed-and-edited.txt");
    assert_eq!(row.status, "synced");
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 2);
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.completed, 0);
}

#[tokio::test]
async fn unpublished_create_then_delete_finishes_offline_and_recreated_path_still_uploads() {
    let fixture = Fixture::new(true);
    let engine = fixture.engine().await;
    std::fs::write(fixture.root.join("brief.txt"), b"short-lived file").unwrap();
    engine.scanner.scan().await.unwrap();
    let file = engine.db.files().await.unwrap().remove(0);
    assert_eq!(file.status, "new_local");
    fixture.drive.offline.store(true, Ordering::SeqCst);
    std::fs::remove_file(fixture.root.join("brief.txt")).unwrap();
    engine.scanner.scan().await.unwrap();
    let retired = engine.db.file(&file.id).await.unwrap();
    assert_eq!(retired.status, "tombstone");
    assert!(retired.fingerprint.is_none());
    assert!(retired.remote_id.is_none());
    assert!(engine.db.operations().await.unwrap().is_empty());
    std::fs::write(
        mygdrive_sync::service::config_path(&fixture.state),
        serde_json::to_vec(&service_config(&fixture)).unwrap(),
    )
    .unwrap();
    let activity = mygdrive_sync::dashboard::read_activity_page(&fixture.state)
        .await
        .unwrap();
    assert_eq!(activity.pending_files, 0);
    assert_eq!(activity.pending_deletions, 0);
    assert!(activity.items.iter().all(|item| !item.pending));
    engine.scanner.scan().await.unwrap();
    assert_eq!(engine.db.file(&file.id).await.unwrap().status, "tombstone");
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 1);

    std::fs::write(fixture.root.join("brief.txt"), b"new intended contents").unwrap();
    engine.scanner.scan().await.unwrap();
    assert_eq!(engine.db.file(&file.id).await.unwrap().status, "new_local");
    fixture.drive.offline.store(false, Ordering::SeqCst);
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    let recreated = engine.db.file(&file.id).await.unwrap();
    assert_eq!(recreated.status, "synced");
    assert_eq!(
        fixture.drive.data(recreated.remote_id.as_deref().unwrap()),
        b"new intended contents"
    );
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn legacy_unpublished_deletions_retire_but_active_journals_and_reviews_are_preserved() {
    let fixture = Fixture::new(true);
    let engine = fixture.engine().await;
    for name in [".~lock.Legacy.docx#", "pending.txt", "review.txt"] {
        std::fs::write(fixture.root.join(name), b"tracked earlier").unwrap();
        engine
            .db
            .record_local(name, Some(fingerprint(&fixture.root.join(name)).unwrap()))
            .await
            .unwrap();
    }
    let files = engine.db.files().await.unwrap();
    let mut legacy = files
        .iter()
        .find(|f| f.relative_path.starts_with(".~lock."))
        .unwrap()
        .clone();
    let pending = files
        .iter()
        .find(|f| f.relative_path == "pending.txt")
        .unwrap();
    let mut review = files
        .iter()
        .find(|f| f.relative_path == "review.txt")
        .unwrap()
        .clone();
    let mut operation = engine
        .db
        .operation(&Operation::new(pending, "create", None))
        .await
        .unwrap();
    legacy.fingerprint = None;
    legacy.status = "deleted_local".into();
    engine.db.save_file(&legacy).await.unwrap();
    review.review_required = true;
    review.status = "recovery_review".into();
    engine.db.save_file(&review).await.unwrap();
    for file in &files {
        std::fs::remove_file(fixture.root.join(&file.relative_path)).unwrap();
    }
    engine.scanner.scan().await.unwrap();
    let repaired = engine.db.file(&legacy.id).await.unwrap();
    assert_eq!(repaired.status, "tombstone");
    assert_eq!(repaired.generation, legacy.generation);
    assert_eq!(
        engine.db.file(&pending.id).await.unwrap().status,
        "deleted_local"
    );
    assert_eq!(
        engine.db.file(&review.id).await.unwrap().status,
        "recovery_review"
    );
    operation.stage = "cancelled".into();
    engine.db.save_operation(&operation).await.unwrap();
    engine.scanner.scan().await.unwrap();
    assert_eq!(
        engine.db.file(&pending.id).await.unwrap().status,
        "tombstone"
    );
    assert_eq!(
        engine.db.file(&review.id).await.unwrap().status,
        "recovery_review"
    );
}

#[tokio::test]
async fn unpublished_deletion_after_lost_create_response_recovers_cloud_identity_before_trash() {
    let fixture = Fixture::new(true);
    let engine = fixture.engine().await;
    std::fs::write(
        fixture.root.join("new.txt"),
        b"created before response was lost",
    )
    .unwrap();
    fixture
        .drive
        .fail_after_create
        .store(true, Ordering::SeqCst);
    assert_eq!(engine.sync_once().await.unwrap().errors.len(), 1);
    let file = engine.db.files().await.unwrap().remove(0);
    let operation = engine.db.operations().await.unwrap().remove(0);
    let remote_id = operation.target_remote_id.clone().unwrap();
    assert!(file.remote_id.is_none());
    std::fs::remove_file(fixture.root.join("new.txt")).unwrap();
    engine.scanner.scan().await.unwrap();
    assert_eq!(
        engine.db.file(&file.id).await.unwrap().status,
        "deleted_local"
    );
    let mut operation = operation;
    operation.next_retry_ms = 0;
    engine.db.save_operation(&operation).await.unwrap();
    drop(engine);
    let engine = fixture.engine().await;
    for _ in 0..3 {
        let report = engine.sync_once().await.unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
    }
    let retired = engine.db.file(&file.id).await.unwrap();
    assert_eq!(retired.status, "tombstone");
    assert_eq!(retired.remote_id.as_deref(), Some(remote_id.as_str()));
    assert!(
        fixture
            .drive
            .get(&remote_id)
            .await
            .unwrap()
            .unwrap()
            .trashed
    );
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 2);
    assert!(!fixture.root.join("new.txt").exists());
    assert!(engine.db.operations().await.unwrap().is_empty());
}

#[tokio::test]
async fn ambiguous_create_is_recovered_after_restart_using_same_remote_id() {
    let fixture = Fixture::new(true);
    let engine = fixture.engine().await;
    std::fs::write(fixture.root.join("new.txt"), b"new upload").unwrap();
    fixture
        .drive
        .fail_after_create
        .store(true, Ordering::SeqCst);
    let report = engine.sync_once().await.unwrap();
    assert_eq!(report.errors.len(), 1);
    for mut op in engine.db.operations().await.unwrap() {
        op.next_retry_ms = 0;
        engine.db.save_operation(&op).await.unwrap();
    }
    drop(engine);
    let engine = fixture.engine().await;
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 2);
    assert_eq!(
        engine
            .db
            .files()
            .await
            .unwrap()
            .iter()
            .filter(|f| f.relative_path == "new.txt")
            .count(),
        1
    );
}

#[tokio::test]
async fn writes_through_old_file_descriptor_are_recovered_after_download() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let mut editor = std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.root.join("note.txt"))
        .unwrap();
    fixture
        .drive
        .put("original", "note.txt", b"server replacement");
    engine.sync_once().await.unwrap();
    editor.write_all(b" + late editor write").unwrap();
    editor.sync_all().unwrap();
    engine.recover_displaced().await.unwrap();
    assert_eq!(
        std::fs::read(fixture.root.join("note.txt")).unwrap(),
        b"server replacement"
    );
    let recovered = engine
        .db
        .files()
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.relative_path.contains("Recovery"))
        .unwrap();
    assert_eq!(
        std::fs::read(fixture.root.join(recovered.relative_path)).unwrap(),
        b"baseline + late editor write"
    );
}

#[tokio::test]
async fn missing_root_and_symlink_never_generate_tombstones() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    std::os::unix::fs::symlink("/tmp", fixture.root.join("outside")).unwrap();
    assert!(engine.scanner.scan().await.is_err());
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "synced"
    );
    std::fs::rename(&fixture.root, fixture._temp.path().join("unmounted")).unwrap();
    assert!(engine.scanner.scan().await.is_err());
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "synced"
    );
}

#[tokio::test]
async fn independent_same_name_and_native_workspace_are_not_overwritten() {
    let fixture = Fixture::new(true);
    fixture.drive.put("original", "note.txt", b"remote");
    std::fs::write(fixture.root.join("note.txt"), b"independent local").unwrap();
    let engine = fixture.engine().await;
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        std::fs::read(fixture.root.join("note.txt")).unwrap(),
        b"independent local"
    );
    assert!(engine
        .db
        .by_remote("original")
        .await
        .unwrap()
        .unwrap()
        .relative_path
        .contains("Drive-"));
}

#[tokio::test]
async fn checksum_mismatch_never_completes_cache_and_access_loss_is_not_delete() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let mut bad = fixture.drive.get("original").await.unwrap().unwrap();
    bad.version = "bad-checksum".into();
    bad.sha256_checksum = Some("wrong".into());
    assert!(engine
        .cache
        .download_to_cache(fixture.drive.as_ref(), &bad)
        .await
        .is_err());
    fixture.drive.files.lock().unwrap().remove("original");
    engine.sync_once().await.unwrap();
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "remote_unavailable"
    );
    assert_eq!(
        std::fs::read(fixture.root.join("note.txt")).unwrap(),
        b"baseline"
    );
}

#[test]
fn reconcile_uses_baseline_and_not_wall_clock() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file");
    std::fs::write(&path, b"base").unwrap();
    let fp = fingerprint(&path).unwrap();
    let remote = RemoteFile {
        id: "id".into(),
        version: "1".into(),
        name: "file".into(),
        sha256_checksum: Some(fp.digest.sha256.clone()),
        ..Default::default()
    };
    let file = LocalFile {
        id: "local".into(),
        relative_path: "file".into(),
        remote_id: Some("id".into()),
        generation: 0,
        fingerprint: Some(fp.clone()),
        baseline: Some(Baseline {
            digest: fp.digest,
            relative_path: "file".into(),
            remote: remote.clone(),
            remote_path: None,
        }),
        status: "synced".into(),
        materialized: true,
        review_required: false,
        available_offline: false,
        metadata_issue: None,
    };
    let mut metadata = remote.clone();
    metadata.version = "2000".into();
    metadata.name = "renamed".into();
    assert_eq!(reconcile(&file, Some(&metadata)), Decision::Rename);
    let mut inaccessible = remote;
    inaccessible.unavailable = true;
    assert_eq!(reconcile(&file, Some(&inaccessible)), Decision::Unavailable);
}

#[tokio::test]
async fn cursor_and_observations_commit_atomically() {
    let fixture = Fixture::new(true);
    let engine = fixture.engine().await;
    engine.db.remote_page(&[], "changes", "old").await.unwrap();
    sqlx::query("CREATE TRIGGER fail_page BEFORE INSERT ON remote_observations WHEN NEW.id='bad' BEGIN SELECT RAISE(ABORT,'fault injection'); END")
        .execute(&engine.db.pool).await.unwrap();
    let good = RemoteFile {
        id: "good".into(),
        ..Default::default()
    };
    let bad = RemoteFile {
        id: "bad".into(),
        ..Default::default()
    };
    assert!(engine
        .db
        .remote_page(&[good, bad], "changes", "new")
        .await
        .is_err());
    assert_eq!(
        engine.db.cursor("changes").await.unwrap().as_deref(),
        Some("old")
    );
    assert!(engine.db.remotes().await.unwrap().is_empty());
}

#[tokio::test]
async fn local_delete_and_rename_remain_pending_without_verified_conditional_mutation() {
    let fixture = Fixture::new(false);
    let engine = fixture.synced().await;
    std::fs::rename(
        fixture.root.join("note.txt"),
        fixture.root.join("renamed.txt"),
    )
    .unwrap();
    engine.sync_once().await.unwrap();
    let file = engine.db.by_remote("original").await.unwrap().unwrap();
    assert_eq!(file.relative_path, "renamed.txt");
    assert_eq!(file.status, "metadata_pending_validation");
    std::fs::remove_file(fixture.root.join("renamed.txt")).unwrap();
    engine.sync_once().await.unwrap();
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "delete_pending_validation"
    );
    assert!(
        !fixture
            .drive
            .get("original")
            .await
            .unwrap()
            .unwrap()
            .trashed
    );
}

#[tokio::test]
async fn confirmed_remote_trash_preserves_inode_in_recovery_and_sets_tombstone() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    {
        let mut files = fixture.drive.files.lock().unwrap();
        let remote = &mut files.get_mut("original").unwrap().0;
        remote.trashed = true;
        remote.version = "2".into();
    }
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(!fixture.root.join("note.txt").exists());
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "tombstone"
    );
    let displaced = engine.db.displaced_files().await.unwrap();
    assert_eq!(std::fs::read(&displaced[0].path).unwrap(), b"baseline");
    engine.sync_once().await.unwrap();
    assert!(!fixture.root.join("note.txt").exists());
}

#[tokio::test]
async fn replay_after_atomic_exchange_before_database_ack_does_not_exchange_back() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let original = engine.db.by_remote("original").await.unwrap().unwrap();
    fixture
        .drive
        .put("original", "note.txt", b"server after crash");
    let remote = fixture.drive.get("original").await.unwrap().unwrap();
    let downloaded = engine
        .cache
        .download_to_cache(fixture.drive.as_ref(), &remote)
        .await
        .unwrap();
    let mut op = Operation::new(&original, "download", Some(remote));
    let target = fixture.root.join("note.txt");
    let staging = fixture
        .root
        .join(format!(".mygdrive-internal-{}.swap", op.id));
    let incoming = mygdrive_sync::local::durable_copy(&downloaded.path, &staging).unwrap();
    let old = fingerprint(&target).unwrap();
    op.downloaded = Some(downloaded);
    op.install = Some(InstallJournal {
        target: target.clone(),
        staging: staging.clone(),
        incoming_inode: incoming.inode,
        old_inode: Some(old.inode),
        old_device: Some(old.device),
        old_digest: Some(old.digest.clone()),
        incoming_digest: incoming.digest,
        exchanged: false,
    });
    engine.db.operation(&op).await.unwrap();
    engine
        .db
        .displaced(&DisplacedFile {
            path: staging.clone(),
            original_relative_path: "note.txt".into(),
            expected: old.digest,
            incoming_inode: incoming.inode,
            promoted_sha256: None,
        })
        .await
        .unwrap();
    let source = CString::new(staging.as_os_str().as_bytes()).unwrap();
    let destination = CString::new(target.as_os_str().as_bytes()).unwrap();
    // SAFETY: private tempdir paths and valid C strings; simulates the crash boundary after syscall.
    assert_eq!(
        unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        },
        0
    );
    drop(engine);
    let engine = fixture.engine().await;
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(std::fs::read(&target).unwrap(), b"server after crash");
    assert_eq!(std::fs::read(&staging).unwrap(), b"baseline");
    assert!(engine.db.operations().await.unwrap().is_empty());
    engine.sync_once().await.unwrap();
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "synced"
    );
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn rejected_cursor_rebuilds_inventory_without_dropping_offline_edit() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    std::fs::write(fixture.root.join("note.txt"), b"offline dirty before reset").unwrap();
    fixture
        .drive
        .reject_cursor_once
        .store(true, Ordering::SeqCst);
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        fixture.drive.data("original"),
        b"offline dirty before reset"
    );
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "synced"
    );
}

#[tokio::test]
async fn status_command_reads_database_while_engine_holds_process_lock() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mygdrive-sync"))
        .arg("--root")
        .arg(&fixture.root)
        .arg("--state")
        .arg(&fixture.state)
        .args(["--account", "test", "status"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let files: Vec<LocalFile> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(files[0].status, "synced");
    assert_eq!(engine.db.files().await.unwrap().len(), 1);
}

#[tokio::test]
async fn server_rename_and_content_change_are_both_applied_without_false_conflict() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    fixture
        .drive
        .put("original", "renamed.txt", b"renamed and edited on server");
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(!fixture.root.join("note.txt").exists());
    assert_eq!(
        std::fs::read(fixture.root.join("renamed.txt")).unwrap(),
        b"renamed and edited on server"
    );
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 2);
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "synced"
    );
}

#[tokio::test]
async fn local_edit_merges_with_server_rename() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    std::fs::write(fixture.root.join("note.txt"), b"local edit survives rename").unwrap();
    fixture.drive.put("original", "renamed.txt", b"baseline");
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        std::fs::read(fixture.root.join("renamed.txt")).unwrap(),
        b"local edit survives rename"
    );
    assert_eq!(
        fixture.drive.data("original"),
        b"local edit survives rename"
    );
    assert_eq!(report.conflicts, 0);
}

#[tokio::test]
async fn server_content_download_does_not_erase_local_rename_intent() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    std::fs::rename(
        fixture.root.join("note.txt"),
        fixture.root.join("local-name.txt"),
    )
    .unwrap();
    fixture.drive.put("original", "note.txt", b"remote content");
    engine.sync_once().await.unwrap();
    let file = engine.db.by_remote("original").await.unwrap().unwrap();
    assert_eq!(file.relative_path, "local-name.txt");
    assert_eq!(file.status, "modified_local");
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        fixture.drive.get("original").await.unwrap().unwrap().name,
        "local-name.txt"
    );
    assert_eq!(fixture.drive.data("original"), b"remote content");
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "synced"
    );
}

#[tokio::test]
async fn server_folder_rename_moves_child_even_when_child_version_does_not_change() {
    let fixture = Fixture::new(true);
    fixture
        .drive
        .create_folder("dir", "old-folder", "root")
        .await
        .unwrap();
    fixture.drive.put("nested", "note.txt", b"nested content");
    fixture
        .drive
        .files
        .lock()
        .unwrap()
        .get_mut("nested")
        .unwrap()
        .0
        .parents = vec!["dir".into()];
    let engine = fixture.engine().await;
    engine.sync_once().await.unwrap();
    let folder = fixture.drive.get("dir").await.unwrap().unwrap();
    fixture
        .drive
        .rename(&folder, "new-folder", "root")
        .await
        .unwrap();
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        std::fs::read(fixture.root.join("new-folder/note.txt")).unwrap(),
        b"nested content"
    );
    assert!(!fixture.root.join("old-folder/note.txt").exists());
    assert_eq!(
        engine
            .db
            .by_remote("nested")
            .await
            .unwrap()
            .unwrap()
            .relative_path,
        "new-folder/note.txt"
    );
}

#[tokio::test]
async fn remote_move_replays_after_crash_before_sql_commit() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let file = engine.db.by_remote("original").await.unwrap().unwrap();
    fixture.drive.put("original", "renamed.txt", b"baseline");
    let remote = fixture.drive.get("original").await.unwrap().unwrap();
    let mut op = Operation::new(&file, "remote_move", Some(remote));
    op.projected_path = Some("renamed.txt".into());
    let fp = file.fingerprint.as_ref().unwrap();
    op.move_journal = Some(MoveJournal {
        source: "note.txt".into(),
        target: "renamed.txt".into(),
        device: fp.device,
        inode: fp.inode,
    });
    engine.db.operation(&op).await.unwrap();
    std::fs::rename(
        fixture.root.join("note.txt"),
        fixture.root.join("renamed.txt"),
    )
    .unwrap();
    // A new user file appears at the old path while the daemon is down.
    std::fs::write(fixture.root.join("note.txt"), b"independent new user file").unwrap();
    drop(engine);
    let engine = fixture.engine().await;
    engine.recover_moves().await.unwrap();
    engine.scanner.scan().await.unwrap();
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .relative_path,
        "renamed.txt"
    );
    assert_eq!(
        std::fs::read(fixture.root.join("note.txt")).unwrap(),
        b"independent new user file"
    );
    assert_eq!(engine.db.files().await.unwrap().len(), 2);
    assert!(engine.db.operations().await.unwrap().is_empty());
}

#[tokio::test]
async fn recovery_review_survives_further_edits_and_moves_until_explicit_approval() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let mut editor = std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.root.join("note.txt"))
        .unwrap();
    fixture.drive.put("original", "note.txt", b"new server");
    engine.sync_once().await.unwrap();
    editor.write_all(b" late edit").unwrap();
    editor.sync_all().unwrap();
    engine.recover_displaced().await.unwrap();
    let recovered = engine
        .db
        .files()
        .await
        .unwrap()
        .into_iter()
        .find(|file| file.review_required)
        .unwrap();
    std::fs::write(
        fixture.root.join(&recovered.relative_path),
        b"reviewed and edited recovery",
    )
    .unwrap();
    std::fs::rename(
        fixture.root.join(&recovered.relative_path),
        fixture.root.join("reviewed.txt"),
    )
    .unwrap();
    engine.sync_once().await.unwrap();
    let current = engine.db.file(&recovered.id).await.unwrap();
    assert!(current.review_required);
    assert_eq!(current.status, "recovery_review");
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 2);
    engine.approve_recovery(&current.id).await.unwrap();
    engine.sync_once().await.unwrap();
    let approved = engine.db.file(&current.id).await.unwrap();
    assert!(!approved.review_required);
    assert!(approved.remote_id.is_some());
    assert_eq!(
        fixture.drive.data(approved.remote_id.as_ref().unwrap()),
        b"reviewed and edited recovery"
    );
}

#[tokio::test]
async fn maintenance_recovery_rejects_stale_review_and_ack_replay_cannot_approve_new_edits() {
    use mygdrive_sync::maintenance::{Outcome, Request};
    let f = Fixture::new(true);
    let engine = f.synced().await;
    let mut editor = std::fs::OpenOptions::new()
        .append(true)
        .open(f.root.join("note.txt"))
        .unwrap();
    f.drive.put("original", "note.txt", b"remote replacement");
    engine.sync_once().await.unwrap();
    editor.write_all(b" late recovery edit").unwrap();
    editor.sync_all().unwrap();
    engine.recover_displaced().await.unwrap();
    let reviewed = engine
        .db
        .files()
        .await
        .unwrap()
        .into_iter()
        .find(|file| file.review_required)
        .unwrap();
    let old = Request::ApproveRecovery {
        file_id: reviewed.id.clone(),
        generation: reviewed.generation,
        sha256: reviewed.fingerprint.as_ref().unwrap().digest.sha256.clone(),
    };
    std::fs::write(
        f.root.join(&reviewed.relative_path),
        b"changed after opening review",
    )
    .unwrap();
    assert!(engine
        .maintain(&uuid::Uuid::new_v4().to_string(), &old)
        .await
        .is_err());
    let current = engine.db.file(&reviewed.id).await.unwrap();
    assert!(current.review_required);
    let command = Request::ApproveRecovery {
        file_id: current.id.clone(),
        generation: current.generation,
        sha256: current.fingerprint.as_ref().unwrap().digest.sha256.clone(),
    };
    let id = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        engine.maintain(&id, &command).await.unwrap(),
        (Outcome::RecoveryApproved, false)
    );
    assert_eq!(
        engine.maintain(&id, &command).await.unwrap(),
        (Outcome::RecoveryApproved, true)
    );
    let mut newer = engine.db.file(&reviewed.id).await.unwrap();
    newer.review_required = true;
    newer.status = "recovery_review".into();
    newer.generation += 1;
    engine.db.save_file(&newer).await.unwrap();
    assert_eq!(
        engine.maintain(&id, &command).await.unwrap(),
        (Outcome::RecoveryApproved, true)
    );
    assert!(engine.db.file(&newer.id).await.unwrap().review_required);
    assert!(engine
        .maintain(&id, &Request::PruneCache { max_bytes: 0 })
        .await
        .is_err());
    drop(editor);
    drop(engine);
    let restarted = f.engine().await;
    assert_eq!(
        restarted.maintain(&id, &command).await.unwrap(),
        (Outcome::RecoveryApproved, true)
    );
    assert!(restarted.db.file(&newer.id).await.unwrap().review_required);
}

#[tokio::test]
async fn maintenance_ipc_requires_current_session_and_preserves_pause_and_offline_pins() {
    use mygdrive_sync::{
        maintenance,
        service::{self, Action, Request},
    };
    let f = Fixture::new(true);
    let engine = Arc::new(f.synced().await);
    let file = engine.db.by_remote("original").await.unwrap().unwrap();
    engine.set_available_offline(&file.id, true).await.unwrap();
    let recovery_path = f.root.join("recovery.txt");
    std::fs::write(&recovery_path, b"reviewed recovered content").unwrap();
    engine.scanner.scan().await.unwrap();
    let mut recovery = engine
        .db
        .files()
        .await
        .unwrap()
        .into_iter()
        .find(|file| file.relative_path == "recovery.txt")
        .unwrap();
    recovery.status = "recovery_review".into();
    recovery.review_required = true;
    engine.db.save_file(&recovery).await.unwrap();
    let mut task = tokio::spawn(service::run_owned(
        engine.clone(),
        f.state.clone(),
        service_config(&f),
        true,
        CancellationToken::new(),
    ));
    let status = tokio::select! {
        result = &mut task => panic!("maintenance service exited early: {result:?}"),
        status = service_wait(&f.state, |status| status.paused) => status,
    };
    let mut request = Request::new(Action::Maintenance);
    request.maintenance = Some(maintenance::Request::PruneCache { max_bytes: 0 });
    assert!(!service::send(&f.state, &request).await.unwrap().accepted);
    request.session_id = Some("old-session".into());
    assert!(!service::send(&f.state, &request).await.unwrap().accepted);
    request.session_id = Some(status.session_id.clone());
    request.offline = Some(mygdrive_sync::offline::Request::List {
        parent: "root".into(),
        offset: 0,
    });
    assert!(!service::send(&f.state, &request).await.unwrap().accepted);
    request.offline = None;
    let reply = service::send(&f.state, &request).await.unwrap();
    assert!(reply.accepted);
    assert!(!reply.duplicate);
    assert!(reply.status.paused);
    assert!(matches!(
        reply.maintenance,
        Some(maintenance::Outcome::CachePruned {
            removed_objects: 0,
            ..
        })
    ));
    let duplicate = service::send(&f.state, &request).await.unwrap();
    assert!(duplicate.accepted && duplicate.duplicate);
    let mut approve = Request::new(Action::Maintenance);
    approve.session_id = Some(status.session_id.clone());
    approve.maintenance = Some(maintenance::Request::ApproveRecovery {
        file_id: recovery.id.clone(),
        generation: recovery.generation,
        sha256: recovery.fingerprint.as_ref().unwrap().digest.sha256.clone(),
    });
    let approved = service::send(&f.state, &approve).await.unwrap();
    assert!(approved.accepted && approved.status.paused);
    assert!(matches!(
        approved.maintenance,
        Some(maintenance::Outcome::RecoveryApproved)
    ));
    assert!(!engine.db.file(&recovery.id).await.unwrap().review_required);
    assert!(engine
        .db
        .file(&recovery.id)
        .await
        .unwrap()
        .remote_id
        .is_none());
    assert!(!engine.db.cached_objects().await.unwrap().is_empty());
    assert_eq!(std::fs::read(f.root.join("note.txt")).unwrap(), b"baseline");
    let page = maintenance::browse(&f.state, 0, true).await.unwrap();
    assert!(page.live);
    assert_eq!(
        page.storage.as_ref().unwrap().skipped,
        0,
        "service socket is not missing file data"
    );
    service::send(&f.state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn pinned_dirty_and_active_cache_objects_are_not_evicted() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let file = engine.db.by_remote("original").await.unwrap().unwrap();
    engine.set_available_offline(&file.id, true).await.unwrap();
    let report = engine.prune_cache(0).await.unwrap();
    assert!(!report.budget_satisfied);
    assert_eq!(report.removed_objects, 0);
    engine.set_available_offline(&file.id, false).await.unwrap();
    std::fs::write(fixture.root.join("note.txt"), b"dirty local").unwrap();
    assert_eq!(engine.prune_cache(0).await.unwrap().removed_objects, 0);
    std::fs::write(fixture.root.join("note.txt"), b"baseline").unwrap();
    engine.scanner.scan().await.unwrap();
    let object = engine.db.cached_objects().await.unwrap().pop().unwrap();
    let current = engine.db.file(&file.id).await.unwrap();
    let mut op = Operation::new(
        &current,
        "download",
        Some(fixture.drive.get("original").await.unwrap().unwrap()),
    );
    op.downloaded = Some(object.snapshot.clone());
    engine.db.operation(&op).await.unwrap();
    assert_eq!(engine.prune_cache(0).await.unwrap().removed_objects, 0);
    op.stage = "cancelled".into();
    engine.db.save_operation(&op).await.unwrap();
    let outbox = engine
        .cache
        .snapshot(&fixture.root.join("note.txt"))
        .await
        .unwrap();
    let report = engine.prune_cache(0).await.unwrap();
    assert_eq!(report.removed_objects, 1);
    assert!(report.budget_satisfied);
    assert!(outbox.path.exists());
    assert_eq!(
        std::fs::read(fixture.root.join("note.txt")).unwrap(),
        b"baseline"
    );
}

#[tokio::test]
async fn cache_eviction_uses_lru_order_and_recovers_missing_object_records() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    for (i, bytes) in [(2, b"two".as_slice()), (3, b"third".as_slice())] {
        fixture.drive.put("original", "note.txt", bytes);
        let remote = fixture.drive.get("original").await.unwrap().unwrap();
        engine
            .cache
            .download_to_cache(fixture.drive.as_ref(), &remote)
            .await
            .unwrap();
        sqlx::query("UPDATE cache_objects SET last_access_ms=? WHERE remote_version=?")
            .bind(i as i64)
            .bind(i.to_string())
            .execute(&engine.db.pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE cache_objects SET last_access_ms=1 WHERE remote_version='1'")
        .execute(&engine.db.pool)
        .await
        .unwrap();
    let old = engine.db.cached_objects().await.unwrap()[0]
        .snapshot
        .path
        .clone();
    let report = engine.prune_cache(8).await.unwrap();
    assert_eq!(report.removed_objects, 1);
    assert!(!old.exists());
    assert_eq!(report.after_bytes, 8);
    let object = engine.db.cached_objects().await.unwrap()[0]
        .snapshot
        .path
        .clone();
    std::fs::remove_file(object).unwrap();
    engine.prune_cache(u64::MAX).await.unwrap();
    assert_eq!(engine.db.cached_objects().await.unwrap().len(), 1);
}

#[tokio::test]
async fn schema_one_cache_records_migrate_without_losing_data() {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("old.sqlite3");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0001_sync.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO settings(key,value) VALUES('account','test')")
        .execute(&pool)
        .await
        .unwrap();
    let source = temp.path().join("blob");
    std::fs::write(&source, b"legacy cache").unwrap();
    let snapshot = Snapshot {
        path: source.clone(),
        digest: fingerprint(&source).unwrap().digest,
    };
    sqlx::query("INSERT INTO cache_objects(key,data,complete) VALUES(?,?,1)")
        .bind("a".repeat(64))
        .bind(serde_json::to_string(&snapshot).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let db = mygdrive_sync::db::Database::open(&path, "test")
        .await
        .unwrap();
    let objects = db.cached_objects().await.unwrap();
    assert_eq!(objects.len(), 1);
    assert!(objects[0].remote_id.is_none());
    assert_eq!(
        std::fs::read(&objects[0].snapshot.path).unwrap(),
        b"legacy cache"
    );
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(version, 4);
}

#[tokio::test]
async fn conflict_copy_of_maximum_length_utf8_filename_preserves_both_versions() {
    let fixture = Fixture::new(true);
    let name = format!("{}.txt", "é".repeat(125));
    assert_eq!(name.len(), 254);
    fixture.drive.put("long", &name, b"base");
    let engine = fixture.engine().await;
    engine.sync_once().await.unwrap();
    std::fs::write(fixture.root.join(&name), b"local").unwrap();
    fixture.drive.put("long", &name, b"server");
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.conflicts, 1);
    let copy = engine
        .db
        .files()
        .await
        .unwrap()
        .into_iter()
        .find(|file| file.relative_path.contains("Conflict"))
        .unwrap();
    assert!(copy.relative_path.len() <= 255);
    assert!(copy.relative_path.ends_with(".txt"));
    assert_eq!(
        std::fs::read(fixture.root.join(&copy.relative_path)).unwrap(),
        b"local"
    );
    assert_eq!(std::fs::read(fixture.root.join(&name)).unwrap(), b"server");
}

#[tokio::test]
async fn remote_rename_collision_preserves_independent_local_file() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    std::fs::write(fixture.root.join("taken.txt"), b"independent local content").unwrap();
    fixture.drive.put("original", "taken.txt", b"baseline");
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        std::fs::read(fixture.root.join("taken.txt")).unwrap(),
        b"independent local content"
    );
    let original = engine.db.by_remote("original").await.unwrap().unwrap();
    assert_ne!(original.relative_path, "taken.txt");
    assert_eq!(
        std::fs::read(fixture.root.join(original.relative_path)).unwrap(),
        b"baseline"
    );
}

#[tokio::test]
async fn divergent_simultaneous_renames_keep_both_intents_for_review() {
    let fixture = Fixture::new(false);
    let engine = fixture.synced().await;
    std::fs::rename(
        fixture.root.join("note.txt"),
        fixture.root.join("local-name.txt"),
    )
    .unwrap();
    fixture
        .drive
        .put("original", "server-name.txt", b"baseline");
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "metadata_conflict_review"
    );
    assert_eq!(
        std::fs::read(fixture.root.join("local-name.txt")).unwrap(),
        b"baseline"
    );
    assert_eq!(
        fixture.drive.get("original").await.unwrap().unwrap().name,
        "server-name.txt"
    );
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn offline_admin_commands_pin_unpin_and_prune_without_oauth() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let id = engine.db.by_remote("original").await.unwrap().unwrap().id;
    drop(engine);
    let invoke = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_mygdrive-sync"))
            .arg("--root")
            .arg(&fixture.root)
            .arg("--state")
            .arg(&fixture.state)
            .args(["--account", "test"])
            .args(args)
            .output()
            .unwrap()
    };
    for args in [
        vec!["pin", &id],
        vec!["unpin", &id],
        vec!["cache-prune", "--max-bytes", "0"],
    ] {
        let result = invoke(&args);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let engine = fixture.engine().await;
    assert!(!engine.db.file(&id).await.unwrap().available_offline);
    assert!(engine.db.cached_objects().await.unwrap().is_empty());
    assert_eq!(
        std::fs::read(fixture.root.join("note.txt")).unwrap(),
        b"baseline"
    );
}

#[tokio::test]
async fn cache_pruning_rejects_records_that_point_to_user_files() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let mut object = engine.db.cached_objects().await.unwrap().pop().unwrap();
    object.snapshot.path = fixture.root.join("note.txt");
    sqlx::query("UPDATE cache_objects SET data=? WHERE key=?")
        .bind(serde_json::to_string(&object.snapshot).unwrap())
        .bind(&object.key)
        .execute(&engine.db.pool)
        .await
        .unwrap();
    assert!(engine.prune_cache(0).await.is_err());
    assert_eq!(
        std::fs::read(fixture.root.join("note.txt")).unwrap(),
        b"baseline"
    );
}

fn service_config(f: &Fixture) -> mygdrive_sync::service::ServiceConfig {
    mygdrive_sync::service::ServiceConfig {
        version: 1,
        account: "test".into(),
        root: f.root.canonicalize().unwrap(),
        drive_folder: "root".into(),
        interval_seconds: 3600,
        cache_max_bytes: None,
    }
}
async fn service_wait(
    state: &std::path::Path,
    check: impl Fn(&mygdrive_sync::service::ServiceStatus) -> bool,
) -> mygdrive_sync::service::ServiceStatus {
    use mygdrive_sync::service::{self, Action, Request};
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(reply) = service::send(state, &Request::new(Action::Status)).await {
                if check(&reply.status) {
                    return reply.status;
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("service state did not converge")
}

#[tokio::test]
async fn reauthentication_retries_without_backoff_and_preserves_pause() {
    use mygdrive_sync::{
        connectivity::ConnectionState,
        onboarding,
        service::{self, Action, Request},
    };
    let f = Fixture::new(true);
    f.drive.auth_required.store(true, Ordering::SeqCst);
    let task = tokio::spawn(service::run_owned(
        Arc::new(f.engine().await),
        f.state.clone(),
        service_config(&f),
        true,
        CancellationToken::new(),
    ));
    let initial = service_wait(&f.state, |s| s.paused).await;
    onboarding::notify_reauthenticated(&f.state).await.unwrap();
    let paused = service::send(&f.state, &Request::new(Action::Status))
        .await
        .unwrap()
        .status;
    assert!(paused.paused && !paused.sync_requested);
    assert_eq!(paused.cycles_finished, 0);
    service::send(&f.state, &Request::new(Action::Resume))
        .await
        .unwrap();
    let failed = service_wait(&f.state, |s| {
        !s.cycle_active && s.connection == ConnectionState::AuthRequired
    })
    .await;
    assert_eq!(failed.last_error.as_deref(), Some("auth_required"));
    f.drive.auth_required.store(false, Ordering::SeqCst);
    onboarding::notify_reauthenticated(&f.state).await.unwrap();
    let recovered = service_wait(&f.state, |s| {
        !s.cycle_active && s.connection == ConnectionState::Online && s.last_error.is_none()
    })
    .await;
    assert_eq!(recovered.session_id, initial.session_id);
    assert!(recovered.cycles_finished > failed.cycles_finished);
    service::send(&f.state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
    // Login remains valid even when there is no running sync service to notify.
    assert!(onboarding::notify_reauthenticated(&f.state).await.is_err());
}
#[tokio::test]
async fn folder_emblems_require_inherited_pin_and_all_content_available_before_green() {
    use mygdrive_sync::{
        availability::{self, FolderBadge},
        cloud_view::View,
    };
    let f = Fixture::new(true);
    put_folder(&f.drive, "parent", "Pinned", "root");
    put_folder(&f.drive, "nested", "Nested", "parent");
    put_folder(&f.drive, "online", "Online", "root");
    f.drive.put("blob", "note.txt", b"verified offline bytes");
    move_under(&f.drive, "blob", "nested");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    let view = View::load(&e.db).await.unwrap();
    assert_eq!(view.folder_badges["Pinned"], FolderBadge::Online);
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:parent", true)
        .await
        .unwrap();
    let view = View::load(&e.db).await.unwrap();
    assert_eq!(view.folder_badges["Pinned"], FolderBadge::Preparing);
    assert_eq!(view.folder_badges["Pinned/Nested"], FolderBadge::Preparing);
    assert_eq!(view.folder_badges["Online"], FolderBadge::Online);
    e.sync_once().await.unwrap();
    let view = View::load(&e.db).await.unwrap();
    assert_eq!(
        std::fs::read(f.root.join("Pinned/Nested/note.txt")).unwrap(),
        b"verified offline bytes"
    );
    assert_eq!(view.folder_badges["Pinned"], FolderBadge::Offline);
    assert_eq!(view.folder_badges["Pinned/Nested"], FolderBadge::Offline);
    let rules = [("r:parent".into(), true)].into_iter().collect();
    let present = [("Pinned/Nested/note.txt".into())].into_iter().collect();
    let incomplete = availability::folder_badges(
        &view.entries,
        &e.db.files().await.unwrap(),
        &rules,
        &present,
        false,
    );
    assert_eq!(incomplete["Pinned"], FolderBadge::Preparing);
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:parent", false)
        .await
        .unwrap();
    let view = View::load(&e.db).await.unwrap();
    assert_eq!(
        view.folder_badges["Pinned"],
        FolderBadge::Online,
        "cached content alone must not claim offline pin"
    );
    assert_eq!(view.folder_badges["Pinned/Nested"], FolderBadge::Online);
    assert!(f.root.join("Pinned/Nested/note.txt").exists());
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:parent", true)
        .await
        .unwrap();
    f.drive.put("native", "Native document", b"workspace");
    {
        let mut files = f.drive.files.lock().unwrap();
        let (native, _) = files.get_mut("native").unwrap();
        native.mime_type = "application/vnd.google-apps.document".into();
        native.parents = vec!["nested".into()];
    }
    e.sync_once().await.unwrap();
    assert_eq!(
        View::load(&e.db).await.unwrap().folder_badges["Pinned"],
        FolderBadge::Preparing
    );
}

#[tokio::test]
async fn dirty_uploads_precede_clean_checks_and_server_changes_still_download() {
    let f = Fixture::new(true);
    f.drive.put("a-clean", "a-clean.txt", b"clean");
    f.drive.put("z-edit", "z-edit.txt", b"baseline");
    let e = f.engine().await;
    e.sync_once().await.unwrap();
    f.drive.get_calls.lock().unwrap().clear();
    std::fs::write(f.root.join("z-edit.txt"), b"first edit").unwrap();
    e.sync_once().await.unwrap();
    let calls = f.drive.get_calls.lock().unwrap().clone();
    assert_eq!(
        calls.iter().find(|id| id.as_str() != "root").unwrap(),
        "z-edit"
    );
    assert_eq!(f.drive.data("z-edit"), b"first edit");
    f.drive.put("a-clean", "a-clean.txt", b"new server content");
    std::fs::write(f.root.join("z-edit.txt"), b"second edit").unwrap();
    f.drive.get_calls.lock().unwrap().clear();
    e.sync_once().await.unwrap();
    let calls = f.drive.get_calls.lock().unwrap().clone();
    assert_eq!(
        calls.iter().find(|id| id.as_str() != "root").unwrap(),
        "z-edit",
        "server checks overtook a pending local save"
    );
    assert!(calls.iter().any(|id| id == "a-clean"));
    assert_eq!(
        std::fs::read(f.root.join("a-clean.txt")).unwrap(),
        b"new server content"
    );
    assert_eq!(f.drive.data("z-edit"), b"second edit");
}

#[tokio::test]
async fn stable_scans_and_identical_file_saves_do_not_invalidate_sqlite_observers() {
    let f = Fixture::new(true);
    let e = f.synced().await;
    let observer = mygdrive_sync::db::Database::read_only(&f.state.join("state.sqlite3"), "test")
        .await
        .unwrap();
    let before: i64 = sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(&observer.pool)
        .await
        .unwrap();
    let file = e.db.by_remote("original").await.unwrap().unwrap();
    for _ in 0..3 {
        e.db.save_file(&file).await.unwrap();
        e.scanner.scan().await.unwrap();
    }
    let stable: i64 = sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(&observer.pool)
        .await
        .unwrap();
    assert_eq!(
        before, stable,
        "unchanged scans must not trigger full namespace refreshes"
    );
    std::fs::write(f.root.join("note.txt"), b"real edit").unwrap();
    e.scanner.scan().await.unwrap();
    let changed: i64 = sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(&observer.pool)
        .await
        .unwrap();
    assert_ne!(stable, changed);
    assert_eq!(
        e.db.by_remote("original").await.unwrap().unwrap().status,
        "modified_local"
    );
}

#[tokio::test]
async fn local_edits_and_empty_folder_mutations_wake_sync_without_interval_polling() {
    use mygdrive_sync::service;
    let f = Fixture::new(true);
    let engine = Arc::new(f.synced().await);
    let stop = CancellationToken::new();
    let task = tokio::spawn(service::run_owned(
        engine,
        f.state.clone(),
        service_config(&f),
        false,
        stop.clone(),
    ));
    let mut status = service_wait(&f.state, |s| s.cycles_finished > 0 && !s.cycle_active).await;
    for bytes in [b"first save".as_slice(), b"second save".as_slice()] {
        std::fs::write(f.root.join("note.txt"), bytes).unwrap();
        status = service_wait(&f.state, |s| {
            s.cycles_finished > status.cycles_finished && !s.cycle_active
        })
        .await;
        assert_eq!(f.drive.data("original"), bytes);
    }
    std::fs::create_dir(f.root.join("wake-empty")).unwrap();
    status = service_wait(&f.state, |s| {
        s.cycles_finished > status.cycles_finished && !s.cycle_active
    })
    .await;
    let directory = f
        .drive
        .list_children("root")
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.name == "wake-empty" && r.folder())
        .unwrap();
    std::fs::rename(f.root.join("wake-empty"), f.root.join("wake-renamed")).unwrap();
    status = service_wait(&f.state, |s| {
        s.cycles_finished > status.cycles_finished && !s.cycle_active
    })
    .await;
    assert_eq!(
        f.drive.get(&directory.id).await.unwrap().unwrap().name,
        "wake-renamed"
    );
    std::fs::remove_dir(f.root.join("wake-renamed")).unwrap();
    status = service_wait(&f.state, |s| {
        s.cycles_finished > status.cycles_finished && !s.cycle_active
    })
    .await;
    assert!(f.drive.get(&directory.id).await.unwrap().unwrap().trashed);
    std::fs::remove_file(f.root.join("note.txt")).unwrap();
    status = service_wait(&f.state, |s| {
        s.cycles_finished > status.cycles_finished && !s.cycle_active
    })
    .await;
    assert!(f.drive.get("original").await.unwrap().unwrap().trashed);
    // Internal installs/acknowledgements and periodic scans must not keep waking sync.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let idle = service_wait(&f.state, |s| !s.cycle_active).await;
    assert_eq!(idle.cycles_finished, status.cycles_finished);
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn service_paused_watcher_records_edits_and_restores_pause_after_restart() {
    use mygdrive_sync::service::{self, Action, Request};
    let f = Fixture::new(true);
    let engine = Arc::new(f.synced().await);
    let config = service_config(&f);
    let state = f.state.clone();
    let stop = CancellationToken::new();
    let task = tokio::spawn(service::run_owned(
        engine.clone(),
        state.clone(),
        config,
        true,
        stop,
    ));
    let initial = service_wait(&state, |s| s.paused).await;
    assert_eq!(initial.cycles_finished, 0);
    assert!(
        SyncEngine::open(&f.root, &f.state, "test", "root", f.drive.clone())
            .await
            .is_err()
    );
    std::fs::write(f.root.join("note.txt"), b"edited while network paused").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if engine
                .db
                .by_remote("original")
                .await
                .unwrap()
                .unwrap()
                .status
                == "modified_local"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.drive.data("original"), b"baseline");
    let rejected = service::send(&state, &Request::new(Action::SyncNow))
        .await
        .unwrap();
    assert!(!rejected.accepted);
    let mut wrong = Request::new(Action::Resume);
    wrong.session_id = Some("stale-session".into());
    assert!(!service::send(&state, &wrong).await.unwrap().accepted);
    let resume = Request::new(Action::Resume);
    assert!(service::send(&state, &resume).await.unwrap().accepted);
    assert!(service::send(&state, &resume).await.unwrap().duplicate);
    service_wait(&state, |s| s.cycles_finished == 1 && !s.cycle_active).await;
    assert_eq!(f.drive.data("original"), b"edited while network paused");
    assert!(
        service::send(&state, &Request::new(Action::Pause))
            .await
            .unwrap()
            .accepted
    );
    assert!(
        service::send(&state, &Request::new(Action::Shutdown))
            .await
            .unwrap()
            .accepted
    );
    task.await.unwrap().unwrap();
    drop(engine);
    assert!(!service::socket_path(&state).exists());
    let old = service::last_status(&state).unwrap();
    assert!(!old.running);
    assert!(old.paused);
    let saved = service::load_config(&state).unwrap();
    let engine = Arc::new(f.engine().await);
    let restart = tokio::spawn(service::run_owned(
        engine,
        state.clone(),
        saved,
        false,
        CancellationToken::new(),
    ));
    let restarted = service_wait(&state, |s| s.paused).await;
    assert_ne!(initial.session_id, restarted.session_id);
    assert_eq!(restarted.cycles_finished, 0);
    service::send(&state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    restart.await.unwrap().unwrap();
}
#[tokio::test]
async fn service_pause_and_shutdown_ack_without_cancelling_active_cycle() {
    use mygdrive_sync::service::{self, Action, Request};
    let f = Fixture::new(true);
    f.drive.hold_get.store(true, Ordering::SeqCst);
    let engine = Arc::new(f.engine().await);
    let state = f.state.clone();
    let task = tokio::spawn(service::run_owned(
        engine,
        state.clone(),
        service_config(&f),
        false,
        CancellationToken::new(),
    ));
    tokio::time::timeout(Duration::from_secs(5), f.drive.get_entered.notified())
        .await
        .unwrap();
    let pause = service::send(&state, &Request::new(Action::Pause))
        .await
        .unwrap();
    assert!(pause.accepted && pause.status.cycle_active);
    assert_eq!(pause.status.phase, "pausing");
    let shutdown = service::send(&state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    assert!(shutdown.accepted && shutdown.status.cycle_active);
    assert!(!task.is_finished());
    f.drive.release_get.notify_one();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let saved = service::last_status(&state).unwrap();
    assert_eq!(saved.cycles_finished, 1);
    assert!(saved.stopped_at_ms.is_some());
}
#[tokio::test]
async fn service_rejects_bad_frames_and_coalesces_duplicate_sync_now() {
    use mygdrive_sync::service::{self, Action, Request};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixStream,
    };
    let f = Fixture::new(true);
    let engine = Arc::new(f.engine().await);
    let state = f.state.clone();
    let task = tokio::spawn(service::run_owned(
        engine,
        state.clone(),
        service_config(&f),
        true,
        CancellationToken::new(),
    ));
    service_wait(&state, |_| true).await;
    for payload in [
        b"not JSON\n".to_vec(),
        vec![b'x'; 16 * 1024 + 2],
        b"{\"version\":999}\n".to_vec(),
    ] {
        let mut stream = UnixStream::connect(service::socket_path(&state))
            .await
            .unwrap();
        let _ = stream.write_all(&payload).await;
        let _ = stream.shutdown().await;
        let mut body = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(6), stream.read_to_end(&mut body))
            .await
            .unwrap();
    }
    let mut wrong = Request::new(Action::Status);
    wrong.version = 999;
    assert!(!service::send(&state, &wrong).await.unwrap().accepted);
    let paused = service_wait(&state, |s| s.paused).await;
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(service::socket_path(&state))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(state.join("control"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(paused.running);
    service::send(&state, &Request::new(Action::Resume))
        .await
        .unwrap();
    service_wait(&state, |s| s.cycles_finished >= 1 && !s.cycle_active).await;
    let request = Request::new(Action::SyncNow);
    service::send(&state, &request).await.unwrap();
    service_wait(&state, |s| s.cycles_finished == 2 && !s.cycle_active).await;
    let retry = service::send(&state, &request).await.unwrap();
    assert!(retry.duplicate && retry.accepted);
    assert!(!retry.status.sync_requested);
    let mut reused = request;
    reused.action = Action::Pause;
    assert!(!service::send(&state, &reused).await.unwrap().accepted);
    service::send(&state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn service_cli_recovers_stale_socket_after_crash_and_reloads_config_without_root() {
    use mygdrive_sync::service::{self, Action, Request};
    let f = Fixture::new(true);
    let binary = env!("CARGO_BIN_EXE_mygdrive-sync");
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("--state")
        .arg(&f.state)
        .args(["--account", "test", "--root"])
        .arg(&f.root)
        .args(["run", "--paused", "--interval", "17"])
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().unwrap();
    let first = service_wait(&f.state, |s| s.paused).await;
    assert_eq!(first.config.interval_seconds, 17);
    let duplicate = tokio::process::Command::new(binary)
        .arg("--state")
        .arg(&f.state)
        .args(["--account", "test", "--root"])
        .arg(&f.root)
        .args(["run", "--paused"])
        .output()
        .await
        .unwrap();
    assert!(!duplicate.status.success());
    assert_eq!(
        service_wait(&f.state, |_| true).await.session_id,
        first.session_id
    );
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    assert!(service::socket_path(&f.state).exists());
    let status = tokio::process::Command::new(binary)
        .arg("--state")
        .arg(&f.state)
        .args(["service", "status"])
        .output()
        .await
        .unwrap();
    assert!(status.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(parsed["running"], false);
    assert_eq!(parsed["last_known"]["running"], false);
    assert_eq!(parsed["last_known"]["phase"], "last_known");
    let mut restarted = tokio::process::Command::new(binary)
        .arg("--state")
        .arg(&f.state)
        .args(["--account", "test", "run"])
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let status = service_wait(&f.state, |s| s.session_id != first.session_id).await;
    assert!(status.paused);
    assert_eq!(status.config.interval_seconds, 17);
    service::send(&f.state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), restarted.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!service::socket_path(&f.state).exists());
}
#[tokio::test]
async fn service_refuses_to_unlink_non_socket_control_path() {
    use mygdrive_sync::service;
    let f = Fixture::new(true);
    let engine = Arc::new(f.engine().await);
    let socket = service::socket_path(&f.state);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::write(&socket, b"user data must survive").unwrap();
    assert!(service::run_owned(
        engine,
        f.state.clone(),
        service_config(&f),
        true,
        CancellationToken::new()
    )
    .await
    .is_err());
    assert_eq!(std::fs::read(socket).unwrap(), b"user data must survive");
}

async fn dashboard_wait(
    controller: &mygdrive_sync::dashboard::Controller,
    check: impl Fn(&mygdrive_sync::dashboard::Snapshot) -> bool,
) -> mygdrive_sync::dashboard::Snapshot {
    use mygdrive_sync::dashboard::Event;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            while let Ok(event) = controller.events.try_recv() {
                if let Event::Snapshot(snapshot) = event {
                    if check(&snapshot) {
                        return *snapshot;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("desktop controller status did not converge")
}
#[tokio::test]
async fn desktop_controller_controls_service_and_distinguishes_historical_status() {
    use mygdrive_sync::{
        dashboard::{self, Command},
        service::{self, Action, Request},
    };
    let f = Fixture::new(true);
    let state = f.state.clone();
    let engine = Arc::new(f.engine().await);
    let task = tokio::spawn(service::run_owned(
        engine,
        state.clone(),
        service_config(&f),
        true,
        CancellationToken::new(),
    ));
    service_wait(&state, |s| s.paused).await;
    let controller = dashboard::spawn(
        state.clone(),
        PathBuf::from(env!("CARGO_BIN_EXE_mygdrive-sync")),
    )
    .unwrap();
    let paused = dashboard_wait(&controller, |s| s.live).await;
    assert_eq!(
        paused.title(),
        mygdrive_sync::i18n::tr("Sinkronisasi dijeda")
    );
    assert_eq!(paused.configuration().unwrap().root, f.root);
    controller.commands.try_send(Command::Resume).unwrap();
    let active = dashboard_wait(&controller, |s| {
        s.status
            .as_ref()
            .is_some_and(|s| s.cycles_finished >= 1 && !s.cycle_active)
    })
    .await;
    assert_eq!(active.title(), mygdrive_sync::i18n::tr("Semua tersinkron"));
    // Start while live must not spawn a writer even if supplied a nonexistent binary path.
    dashboard::perform(&state, std::path::Path::new("/nonexistent"), Command::Start)
        .await
        .unwrap();
    drop(controller);
    assert!(
        service::send(&state, &Request::new(Action::Status))
            .await
            .unwrap()
            .status
            .running
    );
    service::send(&state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
    let historical = dashboard::read_snapshot(&state).await;
    assert!(!historical.live);
    assert_eq!(
        historical.title(),
        mygdrive_sync::i18n::tr("Service tidak aktif")
    );
    assert!(historical.status.unwrap().last_success_ms.is_some());
}

#[tokio::test]
async fn desktop_sync_now_downloads_remote_changes_without_waiting_for_poll_and_respects_pause() {
    use mygdrive_sync::{
        dashboard::{self, Command, Controller, Event},
        service,
    };

    async fn completion(controller: &Controller) -> std::result::Result<&'static str, String> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                while let Ok(event) = controller.events.try_recv() {
                    if let Event::Completed(result) = event {
                        return result;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("dashboard command did not complete")
    }

    let f = Fixture::new(true);
    let engine = Arc::new(f.synced().await);
    let stop = CancellationToken::new();
    let task = tokio::spawn(service::run_owned(
        engine,
        f.state.clone(),
        service_config(&f), // One-hour polling prevents the timer from satisfying this test.
        false,
        stop.clone(),
    ));
    let initial = service_wait(&f.state, |s| s.cycles_finished >= 1 && !s.cycle_active).await;
    let controller = dashboard::spawn_observer(
        f.state.clone(),
        PathBuf::from(env!("CARGO_BIN_EXE_mygdrive-sync")),
        false,
    )
    .unwrap();
    dashboard_wait(&controller, |s| s.live).await;
    let downloads = f.drive.download_calls.load(Ordering::SeqCst);
    // A remote-only change cannot wake the local filesystem watcher.
    f.drive
        .put("original", "note.txt", b"remote edit before manual sync");
    controller.commands.try_send(Command::SyncNow).unwrap();
    assert_eq!(
        completion(&controller).await.unwrap(),
        mygdrive_sync::i18n::tr("Sinkronisasi telah dijadwalkan.")
    );
    let synced = dashboard_wait(&controller, |s| {
        s.status
            .as_ref()
            .is_some_and(|s| s.cycles_finished > initial.cycles_finished && !s.cycle_active)
    })
    .await;
    assert_eq!(
        std::fs::read(f.root.join("note.txt")).unwrap(),
        b"remote edit before manual sync"
    );
    assert!(f.drive.download_calls.load(Ordering::SeqCst) > downloads);
    assert_eq!(synced.title(), mygdrive_sync::i18n::tr("Semua tersinkron"));
    let status = synced.status.unwrap();
    assert!(status.last_success_ms.is_some());
    assert!(status.last_report.unwrap().errors.is_empty());

    controller.commands.try_send(Command::Pause).unwrap();
    completion(&controller).await.unwrap();
    let paused = dashboard_wait(&controller, |s| s.status.as_ref().is_some_and(|s| s.paused)).await;
    let cycles = paused.status.unwrap().cycles_finished;
    f.drive
        .put("original", "note.txt", b"remote edit while paused");
    controller.commands.try_send(Command::SyncNow).unwrap();
    assert!(completion(&controller).await.is_err());
    let still_paused = service_wait(&f.state, |s| s.paused && !s.cycle_active).await;
    assert_eq!(still_paused.cycles_finished, cycles);
    assert!(!still_paused.sync_requested);
    assert_eq!(
        std::fs::read(f.root.join("note.txt")).unwrap(),
        b"remote edit before manual sync"
    );
    drop(controller);
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn desktop_start_uses_saved_config_and_preserves_pause() {
    use mygdrive_sync::{
        dashboard::{self, Command},
        service::{self, Action, Request},
    };
    let f = Fixture::new(true);
    let state = f.state.clone();
    let engine = Arc::new(f.engine().await);
    let task = tokio::spawn(service::run_owned(
        engine,
        state.clone(),
        service_config(&f),
        true,
        CancellationToken::new(),
    ));
    service_wait(&state, |s| s.paused).await;
    service::send(&state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_mygdrive-sync"));
    dashboard::perform(&state, binary, Command::Start)
        .await
        .unwrap();
    let first = service_wait(&state, |s| s.paused).await;
    assert_eq!(first.cycles_finished, 0);
    dashboard::perform(&state, binary, Command::Start)
        .await
        .unwrap();
    assert_eq!(service_wait(&state, |_| true).await.pid, first.pid);
    dashboard::perform(&state, binary, Command::ShutdownAndWait)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(6), async {
        while service::socket_path(&state).exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn desktop_quit_waits_for_active_transfer_and_writer_release() {
    use mygdrive_sync::{
        dashboard::{self, Command},
        service,
    };
    let f = Fixture::new(true);
    f.drive.hold_get.store(true, Ordering::SeqCst);
    let engine = Arc::new(f.engine().await);
    let state = f.state.clone();
    let task = tokio::spawn(service::run_owned(
        engine,
        state.clone(),
        service_config(&f),
        false,
        CancellationToken::new(),
    ));
    tokio::time::timeout(Duration::from_secs(5), f.drive.get_entered.notified())
        .await
        .unwrap();
    let quit_state = state.clone();
    let quit = tokio::spawn(async move {
        dashboard::perform(
            &quit_state,
            std::path::Path::new("/nonexistent"),
            Command::ShutdownAndWait,
        )
        .await
    });
    service_wait(&state, |s| s.shutdown_requested).await;
    assert!(!quit.is_finished());
    assert!(!task.is_finished());
    f.drive.release_get.notify_one();
    tokio::time::timeout(Duration::from_secs(6), quit)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    task.await.unwrap().unwrap();
    assert!(!service::socket_path(&state).exists());
    assert!(service::last_status(&state)
        .unwrap()
        .stopped_at_ms
        .is_some());
    dashboard::perform(
        &state,
        std::path::Path::new("/nonexistent"),
        Command::ShutdownAndWait,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn desktop_quit_cannot_claim_writer_stopped_when_control_socket_is_unavailable() {
    use mygdrive_sync::dashboard::{self, Command};
    let f = Fixture::new(true);
    let _engine = f.engine().await;
    assert!(dashboard::perform(
        &f.state,
        std::path::Path::new("/nonexistent"),
        Command::ShutdownAndWait
    )
    .await
    .is_err());
}

async fn online_engine(f: &Fixture) -> SyncEngine {
    SyncEngine::open(&f.root, &f.state, "test", "root", f.drive.clone())
        .await
        .unwrap()
}
fn put_folder(drive: &FakeDrive, id: &str, name: &str, parent: &str) {
    drive.files.lock().unwrap().insert(
        id.into(),
        (
            RemoteFile {
                id: id.into(),
                name: name.into(),
                mime_type: "application/vnd.google-apps.folder".into(),
                version: "1".into(),
                parents: vec![parent.into()],
                ..Default::default()
            },
            vec![],
        ),
    );
}
fn move_under(drive: &FakeDrive, id: &str, parent: &str) {
    drive.files.lock().unwrap().get_mut(id).unwrap().0.parents = vec![parent.into()];
}

#[tokio::test]
async fn online_default_keeps_only_metadata_until_explicit_file_selection() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"one");
    f.drive.put("two", "two.txt", b"two");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read_dir(&f.root).unwrap().count(), 0);
    assert!(e
        .db
        .files()
        .await
        .unwrap()
        .iter()
        .all(|f| f.status == "online_only" && !f.materialized && f.fingerprint.is_none()));
    e.scanner.scan().await.unwrap();
    e.sync_once().await.unwrap();
    assert_eq!(f.drive.files.lock().unwrap().len(), 3);
    assert!(e.db.cached_objects().await.unwrap().is_empty());
    f.drive.offline.store(true, Ordering::SeqCst);
    let id = uuid::Uuid::new_v4().to_string();
    assert!(!e.offline_select(&id, "r:one", true).await.unwrap());
    assert!(e.sync_once().await.is_err());
    drop(e);
    let e = online_engine(&f).await;
    assert!(e.offline_select(&id, "r:one", true).await.unwrap());
    f.drive.offline.store(false, Ordering::SeqCst);
    e.sync_once().await.unwrap();
    assert_eq!(std::fs::read(f.root.join("one.txt")).unwrap(), b"one");
    assert!(!f.root.join("two.txt").exists());
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 1);
    assert!(e.offline_select(&id, "r:one", false).await.is_err());
    let page = e.offline_page("root", 0).await.unwrap();
    assert_eq!(page.current.counts.total, 2);
    assert_eq!(page.current.counts.ready, 1);
}

#[tokio::test]
async fn offline_folder_rule_inherits_nested_and_future_children_and_survives_rename() {
    let f = Fixture::new(true);
    put_folder(&f.drive, "selected", "Same", "root");
    put_folder(&f.drive, "other", "Same", "root");
    put_folder(&f.drive, "inner", "Nested", "selected");
    f.drive.put("child", "child.txt", b"child");
    move_under(&f.drive, "child", "inner");
    f.drive.put("excluded", "excluded.txt", b"excluded");
    move_under(&f.drive, "excluded", "other");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:selected", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    let file = e.db.by_remote("child").await.unwrap().unwrap();
    assert_eq!(
        std::fs::read(f.root.join(&file.relative_path)).unwrap(),
        b"child"
    );
    assert!(
        !e.db
            .by_remote("excluded")
            .await
            .unwrap()
            .unwrap()
            .materialized
    );
    assert!(
        e.offline_page("r:inner", 0)
            .await
            .unwrap()
            .current
            .inherited
    );
    drop(e);
    let e = online_engine(&f).await;
    f.drive
        .files
        .lock()
        .unwrap()
        .get_mut("selected")
        .unwrap()
        .0
        .name = "Renamed".into();
    f.drive.put("new-child", "new.txt", b"new child");
    move_under(&f.drive, "new-child", "inner");
    e.sync_once().await.unwrap();
    let page = e.offline_page("r:selected", 0).await.unwrap();
    assert!(page.current.selected);
    assert_eq!(page.current.counts.ready, 2);
    assert_eq!(e.prune_cache(0).await.unwrap().removed_objects, 0);
    let child = e.db.by_remote("child").await.unwrap().unwrap();
    std::fs::write(f.root.join(&child.relative_path), b"edited offline").unwrap();
    f.drive.offline.store(true, Ordering::SeqCst);
    assert!(e.sync_once().await.is_err());
    assert_eq!(
        e.db.by_remote("child").await.unwrap().unwrap().status,
        "modified_local"
    );
    f.drive.offline.store(false, Ordering::SeqCst);
    e.sync_once().await.unwrap();
    assert_eq!(f.drive.data("child"), b"edited offline");
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:selected", false)
        .await
        .unwrap();
    assert!(e.prune_cache(0).await.unwrap().removed_objects > 0);
    assert!(f
        .root
        .join(
            e.db.by_remote("child")
                .await
                .unwrap()
                .unwrap()
                .relative_path
        )
        .exists());
    assert_eq!(f.drive.data("child"), b"edited offline");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn standalone_file_and_subfolder_pins_download_through_live_controller_and_thunar() {
    use mygdrive_sync::{
        offline_controller,
        service::{self, Action, Request},
        thunar,
    };
    let f = Fixture::new(true);
    put_folder(&f.drive, "parent", "Parent", "root");
    put_folder(&f.drive, "selected", "Selected", "parent");
    put_folder(&f.drive, "nested", "Nested", "selected");
    put_folder(&f.drive, "empty", "Empty", "nested");
    for (id, name, parent) in [
        ("solo", "solo.txt", "parent"),
        ("nested-file", "inside.txt", "nested"),
        ("sibling", "leave-online.txt", "parent"),
    ] {
        f.drive.put(id, name, id.as_bytes());
        move_under(&f.drive, id, parent);
    }
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    let state = f.state.clone();
    let task = tokio::spawn(service::run_owned(
        e.clone(),
        state.clone(),
        service_config(&f),
        false,
        CancellationToken::new(),
    ));
    let initial = service_wait(&state, |s| s.cycles_finished > 0 && !s.cycle_active).await;
    offline_controller::select(&state, "r:solo", true)
        .await
        .unwrap();
    service_wait(&state, |s| {
        s.cycles_finished > initial.cycles_finished
            && !s.cycle_active
            && f.root.join("Parent/solo.txt").exists()
    })
    .await;
    assert_eq!(
        std::fs::read(f.root.join("Parent/solo.txt")).unwrap(),
        b"solo"
    );
    assert!(!f.root.join("Parent/Selected/Nested/inside.txt").exists());
    assert!(!f.root.join("Parent/leave-online.txt").exists());
    let (page, live) = offline_controller::browse(&state, "r:parent", 0)
        .await
        .unwrap();
    assert!(live);
    assert!(!page.service_paused);
    assert!(!page.current.selected && !page.current.effective);
    let solo = page
        .children
        .iter()
        .find(|node| node.key == "r:solo")
        .unwrap();
    assert!(solo.selected && solo.effective && !solo.inherited);
    assert!(!e.offline_page("root", 0).await.unwrap().current.effective);

    let mount = f.root.parent().unwrap().join("online");
    std::fs::create_dir(&mount).unwrap();
    let before_folder = service::send(&state, &Request::new(Action::Status))
        .await
        .unwrap()
        .status;
    thunar::set_paths(
        &state,
        "test",
        Some(&mount),
        &[mount.join("Parent/Selected")],
        true,
    )
    .await
    .unwrap();
    service_wait(&state, |s| {
        s.cycles_finished > before_folder.cycles_finished
            && !s.cycle_active
            && f.root.join("Parent/Selected/Nested/inside.txt").exists()
    })
    .await;
    assert!(f.root.join("Parent/Selected/Nested/Empty").is_dir());
    let (selected, _) = offline_controller::browse(&state, "r:selected", 0)
        .await
        .unwrap();
    assert!(selected.current.selected && selected.current.effective && !selected.current.inherited);
    let (nested, _) = offline_controller::browse(&state, "r:nested", 0)
        .await
        .unwrap();
    assert!(nested.current.inherited && nested.current.effective);
    assert!(
        !e.offline_page("r:parent", 0)
            .await
            .unwrap()
            .current
            .effective
    );
    assert!(!e.offline_page("root", 0).await.unwrap().current.effective);
    assert!(!f.root.join("Parent/leave-online.txt").exists());

    put_folder(&f.drive, "future-empty", "Future empty", "empty");
    f.drive.put("future", "future.txt", b"future descendant");
    move_under(&f.drive, "future", "nested");
    let before_new = service::send(&state, &Request::new(Action::Status))
        .await
        .unwrap()
        .status;
    service::send(&state, &Request::new(Action::SyncNow))
        .await
        .unwrap();
    service_wait(&state, |s| {
        s.cycles_finished > before_new.cycles_finished
            && !s.cycle_active
            && f.root.join("Parent/Selected/Nested/future.txt").exists()
    })
    .await;
    assert!(f
        .root
        .join("Parent/Selected/Nested/Empty/Future empty")
        .is_dir());
    assert_eq!(
        std::fs::read(f.root.join("Parent/Selected/Nested/future.txt")).unwrap(),
        b"future descendant"
    );
    assert!(!f.root.join("Parent/leave-online.txt").exists());
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 3);
    service::send(&state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn standalone_offline_browser_worker_reports_pause_and_downloads_selection_after_resume() {
    use mygdrive_sync::{
        offline_controller::{self, Command, Event, Job},
        service::{self, Action, Request},
    };
    let f = Fixture::new(true);
    put_folder(&f.drive, "parent", "Parent", "root");
    put_folder(&f.drive, "selected", "Selected", "parent");
    f.drive.put("one", "one.txt", b"standalone folder content");
    move_under(&f.drive, "one", "selected");
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    let state = f.state.clone();
    let task = tokio::spawn(service::run_owned(
        e.clone(),
        state.clone(),
        service_config(&f),
        true,
        CancellationToken::new(),
    ));
    service_wait(&state, |s| s.paused).await;
    let controller = offline_controller::spawn(state.clone()).unwrap();
    controller
        .commands
        .send(Job {
            command: Command::Set {
                target: "r:selected".into(),
                pinned: true,
                parent: "r:parent".into(),
                offset: 0,
            },
            revision: 77,
        })
        .await
        .unwrap();
    let page = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match controller.events.try_recv() {
                Ok(Event::Page {
                    page,
                    live,
                    revision: 77,
                }) => {
                    assert!(live);
                    break page;
                }
                Ok(Event::Error {
                    message,
                    revision: 77,
                }) => panic!("{message}"),
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap();
    assert!(page.service_paused);
    assert!(!page.current.effective);
    let selected = page
        .children
        .iter()
        .find(|node| node.key == "r:selected")
        .unwrap();
    assert!(selected.selected && selected.effective && !selected.inherited);
    assert!(!f.root.join("Parent/Selected/one.txt").exists());
    service::send(&state, &Request::new(Action::Resume))
        .await
        .unwrap();
    service_wait(&state, |s| {
        !s.paused
            && s.cycles_finished > 0
            && !s.cycle_active
            && f.root.join("Parent/Selected/one.txt").exists()
    })
    .await;
    let (ready, live) = offline_controller::browse(&state, "r:selected", 0)
        .await
        .unwrap();
    assert!(live && !ready.service_paused);
    assert_eq!(ready.current.counts.ready, 1);
    assert!(
        !e.offline_page("r:parent", 0)
            .await
            .unwrap()
            .current
            .effective
    );
    assert!(!e.offline_page("root", 0).await.unwrap().current.effective);
    controller.stop();
    drop(controller);
    service::send(&state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn offline_readiness_rechecks_local_identity_and_reports_unsupported_content() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"one");
    let mut native = RemoteFile {
        id: "doc".into(),
        name: "Google Doc".into(),
        mime_type: "application/vnd.google-apps.document".into(),
        version: "1".into(),
        parents: vec!["root".into()],
        ..Default::default()
    };
    native.capabilities.can_download = Some(false);
    f.drive
        .files
        .lock()
        .unwrap()
        .insert("doc".into(), (native, vec![]));
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "root", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    let p = e.offline_page("root", 0).await.unwrap();
    assert_eq!(p.current.counts.unsupported, 1);
    assert_eq!(p.current.counts.ready, 1);
    std::fs::write(f.root.join("one.txt"), b"unscanned edit").unwrap();
    assert_eq!(
        e.offline_page("root", 0)
            .await
            .unwrap()
            .current
            .counts
            .ready,
        0
    );
    e.scanner.scan().await.unwrap();
    let p = e.offline_page("root", 0).await.unwrap();
    assert_eq!(p.current.counts.ready, 1);
    assert_eq!(p.current.counts.dirty, 1);
    std::fs::remove_file(f.root.join("one.txt")).unwrap();
    assert_eq!(
        e.offline_page("root", 0)
            .await
            .unwrap()
            .current
            .counts
            .ready,
        0
    );
}

#[tokio::test]
async fn local_file_and_folder_pins_promote_to_remote_identity_after_upload() {
    let f = Fixture::new(true);
    let e = online_engine(&f).await;
    std::fs::create_dir(f.root.join("Drafts")).unwrap();
    std::fs::write(f.root.join("Drafts/new.txt"), b"local").unwrap();
    e.scanner.scan().await.unwrap();
    let file = e.db.files().await.unwrap().pop().unwrap();
    e.offline_select(
        &uuid::Uuid::new_v4().to_string(),
        &format!("l:{}", file.id),
        true,
    )
    .await
    .unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "p:Drafts", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    e.sync_once().await.unwrap();
    let file = e.db.file(&file.id).await.unwrap();
    let remote = file.remote_id.unwrap();
    let folder = f
        .drive
        .files
        .lock()
        .unwrap()
        .get(&remote)
        .unwrap()
        .0
        .parents[0]
        .clone();
    let p = e.offline_page(&format!("r:{folder}"), 0).await.unwrap();
    assert!(p.current.selected);
    assert!(p.children[0].selected);
    f.drive
        .files
        .lock()
        .unwrap()
        .get_mut(&folder)
        .unwrap()
        .0
        .name = "Published".into();
    f.drive.put("later", "later.txt", b"later");
    move_under(&f.drive, "later", &folder);
    e.sync_once().await.unwrap();
    assert_eq!(
        e.offline_page(&format!("r:{folder}"), 0)
            .await
            .unwrap()
            .current
            .counts
            .ready,
        2
    );
}

#[tokio::test]
async fn offline_ipc_accepts_selection_during_transfer_rejects_reused_payload_and_stale_session() {
    use mygdrive_sync::{
        offline::Request as Selection,
        service::{self, Action, Request},
    };
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"one");
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    f.drive.hold_get.store(true, Ordering::SeqCst);
    let state = f.state.clone();
    let task = tokio::spawn(service::run_owned(
        e.clone(),
        state.clone(),
        service_config(&f),
        false,
        CancellationToken::new(),
    ));
    tokio::time::timeout(Duration::from_secs(5), f.drive.get_entered.notified())
        .await
        .unwrap();
    let s = service_wait(&state, |s| s.cycle_active).await;
    let mut req = Request::new(Action::OfflineSet);
    req.session_id = Some(s.session_id);
    req.offline = Some(Selection::Set {
        target: "r:one".into(),
        pinned: true,
    });
    let reply = tokio::time::timeout(Duration::from_secs(2), service::send(&state, &req))
        .await
        .unwrap()
        .unwrap();
    assert!(reply.accepted);
    assert!(service::send(&state, &req).await.unwrap().duplicate);
    let mut changed = req.clone();
    changed.offline = Some(Selection::Set {
        target: "r:one".into(),
        pinned: false,
    });
    assert!(!service::send(&state, &changed).await.unwrap().accepted);
    let mut stale = Request::new(Action::OfflineSet);
    stale.session_id = Some("old-session".into());
    stale.offline = Some(Selection::Set {
        target: "root".into(),
        pinned: true,
    });
    assert!(!service::send(&state, &stale).await.unwrap().accepted);
    assert!(e
        .offline_select(&uuid::Uuid::new_v4().to_string(), "p:../outside", true)
        .await
        .is_err());
    service::send(&state, &Request::new(Action::Pause))
        .await
        .unwrap();
    f.drive.release_get.notify_one();
    service_wait(&state, |s| !s.cycle_active && s.cycles_finished > 0).await;
    let (p, live) = mygdrive_sync::offline_controller::browse(&state, "root", 0)
        .await
        .unwrap();
    assert!(live);
    assert_eq!(p.current.counts.ready, 1);
    service::send(&state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
    drop(e);
    let (p, live) = mygdrive_sync::offline_controller::browse(&state, "root", 0)
        .await
        .unwrap();
    assert!(!live);
    assert_eq!(p.current.counts.ready, 1);
}

#[tokio::test]
async fn offline_pagination_and_schema_two_upgrade_preserve_materialized_files() {
    let f = Fixture::new(true);
    for i in 0..70 {
        f.drive.put(&format!("id-{i}"), "same.txt", b"same");
    }
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    let mut offset = 0;
    let mut keys = std::collections::HashSet::new();
    loop {
        let page = e.offline_page("root", offset).await.unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() < 128 * 1024);
        for n in page.children {
            assert!(keys.insert(n.key));
        }
        match page.next {
            Some(n) => offset = n,
            None => break,
        }
    }
    assert_eq!(keys.len(), 70);
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 0);
    drop(e);
    let legacy = Fixture::new(true);
    let e = legacy.synced().await;
    let file = e.db.by_remote("original").await.unwrap().unwrap();
    e.set_available_offline(&file.id, true).await.unwrap();
    sqlx::raw_sql("DROP TABLE offline_commands; DROP TABLE offline_selections; DELETE FROM settings WHERE key='storage_mode'; PRAGMA user_version=2;").execute(&e.db.pool).await.unwrap();
    drop(e);
    let e = online_engine(&legacy).await;
    assert!(e.offline_page("root", 0).await.unwrap().children[0].selected);
    assert_eq!(
        std::fs::read(legacy.root.join("note.txt")).unwrap(),
        b"baseline"
    );
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:original", false)
        .await
        .unwrap();
    assert_eq!(e.prune_cache(0).await.unwrap().removed_objects, 1);
    assert!(legacy.root.join("note.txt").exists());
    legacy
        .drive
        .put("new", "metadata-only.txt", b"not downloaded");
    e.sync_once().await.unwrap();
    assert!(!legacy.root.join("metadata-only.txt").exists());
    assert!(!legacy.root.join("note.txt").exists());
    assert_eq!(legacy.drive.data("original"), b"baseline");
    assert!(!legacy.drive.get("original").await.unwrap().unwrap().trashed);
}

#[tokio::test]
async fn schema_three_upgrade_adds_dehydration_queue_and_preserves_downloaded_pins() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"existing pinned content");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    let original = e.db.by_remote("one").await.unwrap().unwrap();
    sqlx::raw_sql("DROP TABLE dehydration_requests; PRAGMA user_version=3;")
        .execute(&e.db.pool)
        .await
        .unwrap();
    drop(e);
    let e = online_engine(&f).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&e.db.pool)
        .await
        .unwrap();
    assert_eq!(version, 4);
    assert_eq!(
        e.db.by_remote("one").await.unwrap().unwrap().id,
        original.id
    );
    assert_eq!(
        std::fs::read(f.root.join("one.txt")).unwrap(),
        b"existing pinned content"
    );
    assert!(e.offline_page("root", 0).await.unwrap().children[0].effective);
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 1);
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
        .await
        .unwrap();
    let report = e.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(!f.root.join("one.txt").exists());
    assert_eq!(f.drive.data("one"), b"existing pinned content");
}

#[tokio::test]
async fn unpin_cancels_unstarted_download_and_repin_does_not_reuse_cancelled_job() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"one");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    let file = e.db.by_remote("one").await.unwrap().unwrap();
    let op = Operation::new(
        &file,
        "download",
        Some(f.drive.get("one").await.unwrap().unwrap()),
    );
    e.db.operation(&op).await.unwrap();
    e.sync_once().await.unwrap();
    assert!(e.db.operations().await.unwrap().is_empty());
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 0);
    e.set_available_offline(&file.id, true).await.unwrap();
    e.sync_once().await.unwrap();
    assert_eq!(std::fs::read(f.root.join("one.txt")).unwrap(), b"one");
    std::fs::write(f.root.join("one.txt"), b"dirty offline").unwrap();
    e.scanner.scan().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
        .await
        .unwrap();
    let eviction = e.prune_cache(0).await.unwrap();
    assert_eq!(eviction.removed_objects, 0);
    assert_eq!(
        std::fs::read(f.root.join("one.txt")).unwrap(),
        b"dirty offline"
    );
}

#[tokio::test]
async fn unpin_frees_clean_content_and_target_cache_but_retains_metadata_for_reopen() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"release these bytes");
    f.drive.put("other", "other.txt", b"keep this opened cache");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    let original = e.db.by_remote("one").await.unwrap().unwrap();
    let other = e.db.by_remote("other").await.unwrap().unwrap();
    e.hydrate(&other.id).await.unwrap();
    let downloads = f.drive.download_calls.load(Ordering::SeqCst);
    let request = uuid::Uuid::new_v4().to_string();
    e.offline_select(&request, "r:one", false).await.unwrap();
    let report = e.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(!f.root.join("one.txt").exists());
    assert_eq!(
        std::fs::read(f.root.join("other.txt")).unwrap(),
        b"keep this opened cache"
    );
    let released = e.db.by_remote("one").await.unwrap().unwrap();
    assert_eq!(released.id, original.id);
    assert_eq!(released.remote_id, original.remote_id);
    assert_eq!(released.status, "online_only");
    assert!(!released.materialized);
    assert!(released.fingerprint.is_none());
    assert!(released.baseline.is_some());
    assert_eq!(f.drive.data("one"), b"release these bytes");
    assert!(!f.drive.get("one").await.unwrap().unwrap().trashed);
    let objects = e.db.cached_objects().await.unwrap();
    assert!(!objects
        .iter()
        .any(|o| o.remote_id.as_deref() == Some("one")));
    assert!(objects
        .iter()
        .any(|o| o.remote_id.as_deref() == Some("other")));
    assert!(std::fs::read_dir(&f.root).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(mygdrive_sync::local::INTERNAL_PREFIX)));

    // A subsequent scanner/cycle must not interpret dehydration as a Drive deletion.
    e.scanner.scan().await.unwrap();
    e.sync_once().await.unwrap();
    assert!(!f.drive.get("one").await.unwrap().unwrap().trashed);
    e.hydrate(&original.id).await.unwrap();
    assert_eq!(
        std::fs::read(f.root.join("one.txt")).unwrap(),
        b"release these bytes"
    );
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), downloads + 1);
    assert!(
        !e.offline_page("root", 0)
            .await
            .unwrap()
            .children
            .iter()
            .find(|n| n.key == "r:one")
            .unwrap()
            .effective
    );

    // Replaying the completed IPC request cannot evict a later intentional open.
    assert!(e.offline_select(&request, "r:one", false).await.unwrap());
    e.sync_once().await.unwrap();
    assert!(f.root.join("one.txt").exists());
}

#[tokio::test]
async fn unpin_folder_frees_descendants_but_keeps_explicit_child_pins_and_unrelated_content() {
    let f = Fixture::new(true);
    put_folder(&f.drive, "parent", "Parent", "root");
    put_folder(&f.drive, "nested", "Nested", "parent");
    put_folder(&f.drive, "protected", "Protected", "parent");
    for (id, name, parent) in [
        ("released", "release.txt", "nested"),
        ("pinned-file", "keep.txt", "parent"),
        ("pinned-child", "child.txt", "protected"),
        ("outside", "outside.txt", "root"),
    ] {
        f.drive.put(id, name, id.as_bytes());
        move_under(&f.drive, id, parent);
    }
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    for target in ["r:parent", "r:pinned-file", "r:protected"] {
        e.offline_select(&uuid::Uuid::new_v4().to_string(), target, true)
            .await
            .unwrap();
    }
    e.sync_once().await.unwrap();
    let outside = e.db.by_remote("outside").await.unwrap().unwrap();
    e.hydrate(&outside.id).await.unwrap();
    f.drive.put("future", "future.txt", b"future child");
    move_under(&f.drive, "future", "nested");
    e.sync_once().await.unwrap();

    // A child that still inherits its parent's selection cannot be dehydrated.
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:nested", false)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    assert!(f.root.join("Parent/Nested/release.txt").exists());
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:parent", false)
        .await
        .unwrap();
    let report = e.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    for id in ["released", "future"] {
        let file = e.db.by_remote(id).await.unwrap().unwrap();
        assert!(!f.root.join(&file.relative_path).exists(), "{id}");
        assert_eq!(file.status, "online_only");
    }
    for id in ["pinned-file", "pinned-child", "outside"] {
        let file = e.db.by_remote(id).await.unwrap().unwrap();
        assert!(f.root.join(&file.relative_path).exists(), "{id}");
    }
    let objects = e.db.cached_objects().await.unwrap();
    assert!(objects
        .iter()
        .all(|o| !matches!(o.remote_id.as_deref(), Some("released" | "future"))));
    for id in ["pinned-file", "pinned-child", "outside"] {
        assert!(
            objects.iter().any(|o| o.remote_id.as_deref() == Some(id)),
            "{id}"
        );
    }
}

#[tokio::test]
async fn unpin_waits_for_upload_and_preserves_review_and_pending_operations_across_restart() {
    let f = Fixture::new(true);
    for id in ["dirty", "review", "pending"] {
        f.drive.put(id, &format!("{id}.txt"), b"baseline");
    }
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "root", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    let mut review = e.db.by_remote("review").await.unwrap().unwrap();
    review.review_required = true;
    review.status = "recovery_review".into();
    e.db.save_file(&review).await.unwrap();
    let pending = e.db.by_remote("pending").await.unwrap().unwrap();
    let mut op = Operation::new(&pending, "download", f.drive.get("pending").await.unwrap());
    // The original hydration completed the same generation/version download.
    // Give this intentionally deferred operation its own idempotency key.
    op.key.push_str(":pending-for-unpin-test");
    op.next_retry_ms = i64::MAX;
    let registered = e.db.operation(&op).await.unwrap();
    assert_eq!(registered.id, op.id);
    assert_eq!(registered.stage, "planned");
    std::fs::write(f.root.join("dirty.txt"), b"unsaved local edit").unwrap();
    std::fs::write(f.root.join("new.txt"), b"not yet uploaded").unwrap();
    e.scanner.scan().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "root", false)
        .await
        .unwrap();
    f.drive.offline.store(true, Ordering::SeqCst);
    assert!(e.sync_once().await.is_err());
    for name in ["dirty.txt", "review.txt", "pending.txt", "new.txt"] {
        assert!(f.root.join(name).exists(), "{name}");
    }
    assert_eq!(f.drive.data("dirty"), b"baseline");
    drop(e);
    let e = online_engine(&f).await;
    e.scanner.scan().await.unwrap();
    assert_eq!(
        e.db.by_remote("dirty").await.unwrap().unwrap().status,
        "modified_local"
    );
    f.drive.offline.store(false, Ordering::SeqCst);
    let report = e.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(f.drive.data("dirty"), b"unsaved local edit");
    assert!(!f.root.join("dirty.txt").exists());
    let uploaded =
        e.db.files()
            .await
            .unwrap()
            .into_iter()
            .find(|file| file.relative_path == "new.txt")
            .unwrap();
    assert_eq!(
        f.drive.data(uploaded.remote_id.as_deref().unwrap()),
        b"not yet uploaded"
    );
    assert!(!f.root.join("new.txt").exists());
    assert_eq!(
        std::fs::read(f.root.join("review.txt")).unwrap(),
        b"baseline"
    );
    assert_eq!(
        std::fs::read(f.root.join("pending.txt")).unwrap(),
        b"baseline"
    );
    assert!(e.db.file(&review.id).await.unwrap().review_required);
    assert!(e
        .db
        .operations()
        .await
        .unwrap()
        .iter()
        .any(|current| current.id == op.id));
}

#[tokio::test]
async fn unpin_preserves_open_backing_descriptor_and_its_later_writes_until_close() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"initial");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    let path = f.root.join("one.txt");
    let mut open = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
        .await
        .unwrap();
    let report = e.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"initial");
    open.write_all(b" plus edit").unwrap();
    open.sync_all().unwrap();
    e.sync_once().await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"initial plus edit");
    assert_eq!(f.drive.data("one"), b"initial plus edit");
    drop(open);
    e.sync_once().await.unwrap();
    assert!(!path.exists());
    assert_eq!(f.drive.data("one"), b"initial plus edit");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn unpin_preserves_writable_mapping_even_after_its_descriptor_is_closed() {
    use std::os::fd::AsRawFd;
    struct Mapping(*mut libc::c_void, usize);
    impl Drop for Mapping {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.0, self.1);
            }
        }
    }
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"mapped bytes");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    let path = f.root.join("one.txt");
    let fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let address = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            12,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    assert_ne!(address, libc::MAP_FAILED);
    let mapping = Mapping(address, 12);
    drop(fd);
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
        .await
        .unwrap();
    let report = e.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"mapped bytes");
    unsafe {
        std::ptr::write_volatile(mapping.0.cast::<u8>(), b'M');
        assert_eq!(libc::msync(mapping.0, mapping.1, libc::MS_SYNC), 0);
    }
    drop(mapping);
    e.sync_once().await.unwrap();
    assert_eq!(f.drive.data("one"), b"Mapped bytes");
    assert!(!path.exists());
}

#[tokio::test]
async fn unpin_rechecks_remote_safety_after_reconciliation_and_retries_later() {
    for case in ["missing", "version", "checksum", "permission", "scope"] {
        let f = Fixture::new(true);
        f.drive.put("one", "one.txt", b"verified remote bytes");
        let e = online_engine(&f).await;
        e.sync_once().await.unwrap();
        e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
            .await
            .unwrap();
        e.sync_once().await.unwrap();
        e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
            .await
            .unwrap();
        let mut changed = f.drive.get("one").await.unwrap().unwrap();
        match case {
            "version" => changed.version = "999".into(),
            "checksum" => changed.sha256_checksum = Some("bad checksum".into()),
            "permission" => changed.capabilities.can_download = Some(false),
            "scope" => changed.parents = vec!["outside-profile".into()],
            "missing" => {}
            _ => unreachable!(),
        }
        // Reconciliation observes the original; the later reclamation GET must
        // independently reject this unavailable or changed server response.
        *f.drive.get_override.lock().unwrap() = Some((
            "one".into(),
            1,
            if case == "missing" {
                None
            } else {
                Some(changed)
            },
        ));
        e.sync_once().await.unwrap();
        assert_eq!(
            std::fs::read(f.root.join("one.txt")).unwrap(),
            b"verified remote bytes",
            "{case}"
        );
        assert!(
            e.db.by_remote("one").await.unwrap().unwrap().materialized,
            "{case}"
        );
        assert!(
            e.db.cached_objects()
                .await
                .unwrap()
                .iter()
                .any(|o| o.remote_id.as_deref() == Some("one")),
            "{case}"
        );
        e.sync_once().await.unwrap();
        assert!(!f.root.join("one.txt").exists(), "{case}");
        assert_eq!(f.drive.data("one"), b"verified remote bytes");
    }
}

#[tokio::test]
async fn unpin_cleanup_is_cancelled_by_repin_and_retains_clean_content_without_network() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"keep available");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
        .await
        .unwrap();
    f.drive.offline.store(true, Ordering::SeqCst);
    assert!(e.sync_once().await.is_err());
    assert_eq!(
        std::fs::read(f.root.join("one.txt")).unwrap(),
        b"keep available"
    );
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
        .await
        .unwrap();
    f.drive.offline.store(false, Ordering::SeqCst);
    e.sync_once().await.unwrap();
    assert_eq!(
        std::fs::read(f.root.join("one.txt")).unwrap(),
        b"keep available"
    );
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 1);
    assert!(e.offline_page("root", 0).await.unwrap().children[0].effective);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpin_new_request_survives_completion_of_an_older_cleanup_after_reopen() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"first file");
    f.drive.put("two", "two.txt", b"second file");
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    for target in ["r:one", "r:two"] {
        e.offline_select(&uuid::Uuid::new_v4().to_string(), target, true)
            .await
            .unwrap();
    }
    e.sync_once().await.unwrap();
    let one = e.db.by_remote("one").await.unwrap().unwrap();
    // Requests enter the durable queue in this order. Hold the second file's
    // cleanup verification after its ordinary reconciliation GET has passed.
    for target in ["r:one", "r:two"] {
        e.offline_select(&uuid::Uuid::new_v4().to_string(), target, false)
            .await
            .unwrap();
    }
    *f.drive.hold_get_after.lock().unwrap() = Some(("two".into(), 1));
    let engine = e.clone();
    let task = tokio::spawn(async move { engine.sync_once().await });
    tokio::time::timeout(Duration::from_secs(5), f.drive.get_entered.notified())
        .await
        .unwrap();
    assert!(!f.root.join("one.txt").exists());
    e.hydrate(&one.id).await.unwrap();
    assert_eq!(
        std::fs::read(f.root.join("one.txt")).unwrap(),
        b"first file"
    );
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
        .await
        .unwrap();
    f.drive.release_get.notify_one();
    let report = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(f.root.join("one.txt").exists());
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM dehydration_requests WHERE file_id=?")
            .bind(&one.id)
            .fetch_one(&e.db.pool)
            .await
            .unwrap();
    assert_eq!(
        pending, 1,
        "a newer request must outlive the older worker's completion"
    );
    let report = e.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(!f.root.join("one.txt").exists());
    assert_eq!(f.drive.data("one"), b"first file");
    assert!(!f.drive.get("one").await.unwrap().unwrap().trashed);
}

#[tokio::test]
async fn unpin_recovers_each_interrupted_removal_stage_before_scanning_or_remote_sync() {
    for stage in ["prepared", "renamed", "online", "unlinked"] {
        let f = Fixture::new(true);
        f.drive.put("one", "one.txt", b"recoverable remote bytes");
        let e = online_engine(&f).await;
        e.sync_once().await.unwrap();
        e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
            .await
            .unwrap();
        e.sync_once().await.unwrap();
        let original = e.db.by_remote("one").await.unwrap().unwrap();
        e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
            .await
            .unwrap();
        let staging = format!(
            "{}{}.evict",
            mygdrive_sync::local::INTERNAL_PREFIX,
            uuid::Uuid::new_v4()
        );
        let journal = serde_json::json!({ "file": original, "staging": staging });
        sqlx::query("UPDATE dehydration_requests SET journal=? WHERE file_id=?")
            .bind(serde_json::to_string(&journal).unwrap())
            .bind(&original.id)
            .execute(&e.db.pool)
            .await
            .unwrap();
        let source = f.root.join("one.txt");
        let staged = f.root.join(&staging);
        if stage != "prepared" {
            std::fs::rename(&source, &staged).unwrap();
            mygdrive_sync::local::sync_directory(&f.root).unwrap();
        }
        if matches!(stage, "online" | "unlinked") {
            let mut online = original.clone();
            online.generation += 1;
            online.materialized = false;
            online.fingerprint = None;
            online.available_offline = false;
            online.status = "online_only".into();
            e.db.save_file(&online).await.unwrap();
        }
        if stage == "unlinked" {
            std::fs::remove_file(&staged).unwrap();
            mygdrive_sync::local::sync_directory(&f.root).unwrap();
        }
        drop(e);
        let e = online_engine(&f).await;
        e.scanner.scan().await.unwrap();
        let recovered = e.db.by_remote("one").await.unwrap().unwrap();
        assert_eq!(recovered.id, original.id, "{stage}");
        assert!(
            !matches!(recovered.status.as_str(), "deleted_local" | "tombstone"),
            "{stage}"
        );
        assert!(!staged.exists(), "{stage}");
        if stage == "unlinked" {
            assert!(!source.exists());
            assert_eq!(recovered.status, "online_only");
            assert!(!recovered.materialized);
        } else {
            assert_eq!(
                std::fs::read(&source).unwrap(),
                b"recoverable remote bytes",
                "{stage}"
            );
            assert!(recovered.materialized, "{stage}");
            assert_eq!(recovered.status, "synced", "{stage}");
        }
        let active_journals: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM dehydration_requests WHERE journal IS NOT NULL",
        )
        .fetch_one(&e.db.pool)
        .await
        .unwrap();
        assert_eq!(active_journals, 0, "{stage}");
        // Recovery is idempotent, and the remaining request can finish normally.
        e.scanner.scan().await.unwrap();
        let report = e.sync_once().await.unwrap();
        assert!(report.errors.is_empty(), "{stage}: {report:?}");
        assert_eq!(f.drive.data("one"), b"recoverable remote bytes");
        assert!(
            !f.drive.get("one").await.unwrap().unwrap().trashed,
            "{stage}"
        );
        assert!(!source.exists(), "{stage}");
    }
}

#[tokio::test]
async fn unpin_recovery_preserves_a_recreated_path_and_staged_bytes_for_review() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"original staged content");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    let original = e.db.by_remote("one").await.unwrap().unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
        .await
        .unwrap();
    let staging = format!(
        "{}{}.evict",
        mygdrive_sync::local::INTERNAL_PREFIX,
        uuid::Uuid::new_v4()
    );
    sqlx::query("UPDATE dehydration_requests SET journal=? WHERE file_id=?")
        .bind(serde_json::json!({ "file": original, "staging": staging }).to_string())
        .bind(&original.id)
        .execute(&e.db.pool)
        .await
        .unwrap();
    std::fs::rename(f.root.join("one.txt"), f.root.join(&staging)).unwrap();
    let mut online = original.clone();
    online.generation += 1;
    online.materialized = false;
    online.fingerprint = None;
    online.status = "online_only".into();
    e.db.save_file(&online).await.unwrap();
    std::fs::write(f.root.join("one.txt"), b"new concurrent file").unwrap();
    drop(e);
    let e = online_engine(&f).await;
    e.scanner.scan().await.unwrap();
    assert_eq!(
        std::fs::read(f.root.join("one.txt")).unwrap(),
        b"new concurrent file"
    );
    assert!(!f.root.join(&staging).exists());
    let files = e.db.files().await.unwrap();
    let recovered = files
        .iter()
        .find(|file| file.relative_path != "one.txt")
        .unwrap();
    assert!(recovered.review_required);
    assert_eq!(recovered.status, "recovery_review");
    assert_eq!(
        std::fs::read(f.root.join(&recovered.relative_path)).unwrap(),
        b"original staged content"
    );
    assert_eq!(
        e.db.by_remote("one").await.unwrap().unwrap().id,
        original.id
    );
    assert_eq!(f.drive.data("one"), b"original staged content");
    assert!(!f.drive.get("one").await.unwrap().unwrap().trashed);
    e.scanner.scan().await.unwrap();
    assert_eq!(e.db.files().await.unwrap().len(), 2);
}

#[tokio::test]
async fn unpin_recovery_resumes_interrupted_rollback_before_any_recovered_file_upload() {
    for recorded in [false, true] {
        let f = Fixture::new(true);
        f.drive.put("one", "one.txt", b"recover this for review");
        let e = online_engine(&f).await;
        e.sync_once().await.unwrap();
        e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
            .await
            .unwrap();
        e.sync_once().await.unwrap();
        let original = e.db.by_remote("one").await.unwrap().unwrap();
        e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
            .await
            .unwrap();
        let staging = format!(
            "{}{}.evict",
            mygdrive_sync::local::INTERNAL_PREFIX,
            uuid::Uuid::new_v4()
        );
        let rollback = mygdrive_sync::engine::conflict_name(
            &original.relative_path,
            &format!(
                "Recovery - {}",
                std::path::Path::new(&staging)
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
            ),
        );
        sqlx::query("UPDATE dehydration_requests SET journal=? WHERE file_id=?")
            .bind(
                serde_json::json!({
                    "file": original, "staging": staging, "rollback": rollback
                })
                .to_string(),
            )
            .bind(&original.id)
            .execute(&e.db.pool)
            .await
            .unwrap();
        // The first recovery moved the staged inode to its persisted collision
        // destination, then stopped before its review state was durable.
        std::fs::rename(f.root.join("one.txt"), f.root.join(&rollback)).unwrap();
        let mut online = original.clone();
        online.generation += 1;
        online.materialized = false;
        online.fingerprint = None;
        online.status = "online_only".into();
        e.db.save_file(&online).await.unwrap();
        std::fs::write(f.root.join("one.txt"), b"concurrent replacement").unwrap();
        if recorded {
            e.db.record_local(
                &rollback,
                Some(fingerprint(&f.root.join(&rollback)).unwrap()),
            )
            .await
            .unwrap();
            assert_eq!(
                e.db.files()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|file| file.relative_path == rollback)
                    .unwrap()
                    .status,
                "new_local"
            );
        }
        drop(e);
        let e = online_engine(&f).await;
        let recovered =
            e.db.files()
                .await
                .unwrap()
                .into_iter()
                .find(|file| file.relative_path == rollback)
                .unwrap();
        assert!(recovered.review_required, "recorded={recorded}");
        assert_eq!(recovered.status, "recovery_review");
        e.scanner.scan().await.unwrap();
        let report = e.sync_once().await.unwrap();
        assert!(report.errors.is_empty(), "recorded={recorded}: {report:?}");
        assert_eq!(
            std::fs::read(f.root.join(&rollback)).unwrap(),
            b"recover this for review"
        );
        assert!(e.db.file(&recovered.id).await.unwrap().remote_id.is_none());
        assert!(e.db.file(&recovered.id).await.unwrap().review_required);
        assert!(!f
            .drive
            .files
            .lock()
            .unwrap()
            .values()
            .any(|(remote, _)| remote.name == rollback));
        assert!(!f.drive.get("one").await.unwrap().unwrap().trashed);
    }
}

#[tokio::test]
async fn unpin_recovery_preserves_recreated_original_and_occupied_rollback_destination() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"staged original bytes");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    let original = e.db.by_remote("one").await.unwrap().unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
        .await
        .unwrap();
    let staging = format!(
        "{}{}.evict",
        mygdrive_sync::local::INTERNAL_PREFIX,
        uuid::Uuid::new_v4()
    );
    let default_recovery = mygdrive_sync::engine::conflict_name(
        &original.relative_path,
        &format!(
            "Recovery - {}",
            std::path::Path::new(&staging)
                .file_stem()
                .unwrap()
                .to_string_lossy()
        ),
    );
    // Recovery persisted a rollback to the original pathname, then stopped
    // before its rename. Another app has since occupied both normal choices.
    sqlx::query("UPDATE dehydration_requests SET journal=? WHERE file_id=?")
        .bind(
            serde_json::json!({
                "file": original,
                "staging": staging,
                "rollback": original.relative_path
            })
            .to_string(),
        )
        .bind(&original.id)
        .execute(&e.db.pool)
        .await
        .unwrap();
    std::fs::rename(f.root.join("one.txt"), f.root.join(&staging)).unwrap();
    std::fs::write(f.root.join("one.txt"), b"recreated original pathname").unwrap();
    std::fs::write(
        f.root.join(&default_recovery),
        b"occupied recovery pathname",
    )
    .unwrap();
    drop(e);
    let e = online_engine(&f).await;
    e.scanner.scan().await.unwrap();
    assert_eq!(
        std::fs::read(f.root.join("one.txt")).unwrap(),
        b"recreated original pathname"
    );
    assert_eq!(
        std::fs::read(f.root.join(&default_recovery)).unwrap(),
        b"occupied recovery pathname"
    );
    assert!(!f.root.join(&staging).exists());
    let files = e.db.files().await.unwrap();
    assert_eq!(files.len(), 3);
    let recovered = files.iter().find(|file| file.review_required).unwrap();
    assert_ne!(recovered.relative_path, "one.txt");
    assert_ne!(recovered.relative_path, default_recovery);
    assert_eq!(recovered.status, "recovery_review");
    assert_eq!(
        std::fs::read(f.root.join(&recovered.relative_path)).unwrap(),
        b"staged original bytes"
    );
    assert_eq!(f.drive.data("one"), b"staged original bytes");
    assert!(!f.drive.get("one").await.unwrap().unwrap().trashed);
    e.scanner.scan().await.unwrap();
    assert_eq!(e.db.files().await.unwrap().len(), 3);
}

#[tokio::test]
async fn unpin_does_not_remove_content_when_the_backing_root_identity_changes() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"preserve the original tree");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
        .await
        .unwrap();
    let saved = f.root.with_file_name("original-sync-root");
    std::fs::rename(&f.root, &saved).unwrap();
    std::fs::create_dir(&f.root).unwrap();
    assert!(e.sync_once().await.is_err());
    assert_eq!(
        std::fs::read(saved.join("one.txt")).unwrap(),
        b"preserve the original tree"
    );
    assert!(!f.root.join("one.txt").exists());
    assert!(!f.drive.get("one").await.unwrap().unwrap().trashed);
}

#[tokio::test]
async fn unpin_malformed_recovery_journal_blocks_scanning_without_touching_user_files() {
    for case in [
        "invalid-json",
        "wrong-file",
        "escape",
        "not-internal",
        "other-parent",
    ] {
        let f = Fixture::new(true);
        f.drive.put("one", "one.txt", b"keep original");
        let e = online_engine(&f).await;
        e.sync_once().await.unwrap();
        e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", true)
            .await
            .unwrap();
        e.sync_once().await.unwrap();
        let original = e.db.by_remote("one").await.unwrap().unwrap();
        e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:one", false)
            .await
            .unwrap();
        let mut journal_file = original.clone();
        let name = format!(
            "{}{}.evict",
            mygdrive_sync::local::INTERNAL_PREFIX,
            uuid::Uuid::new_v4()
        );
        let staging = match case {
            "wrong-file" => {
                journal_file.id = "another-file".into();
                name.clone()
            }
            "escape" => format!("../{name}"),
            "not-internal" => "ordinary-user-file.txt".into(),
            "other-parent" => format!("elsewhere/{name}"),
            _ => name.clone(),
        };
        let data = if case == "invalid-json" {
            "{".into()
        } else {
            serde_json::json!({ "file": journal_file, "staging": staging }).to_string()
        };
        sqlx::query("UPDATE dehydration_requests SET journal=? WHERE file_id=?")
            .bind(data)
            .bind(&original.id)
            .execute(&e.db.pool)
            .await
            .unwrap();
        std::fs::write(f.root.join("ordinary-user-file.txt"), b"also keep this").unwrap();
        assert!(e.scanner.scan().await.is_err(), "{case}");
        assert_eq!(
            std::fs::read(f.root.join("one.txt")).unwrap(),
            b"keep original",
            "{case}"
        );
        assert_eq!(
            std::fs::read(f.root.join("ordinary-user-file.txt")).unwrap(),
            b"also keep this",
            "{case}"
        );
        assert_eq!(
            e.db.file(&original.id).await.unwrap().status,
            "synced",
            "{case}"
        );
        assert!(
            !f.drive.get("one").await.unwrap().unwrap().trashed,
            "{case}"
        );
    }
}

#[tokio::test]
async fn offline_recursive_query_searches_parent_index_and_does_not_scan_unrelated_account_items() {
    use sqlx::Row;
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"one");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    let unrelated: Vec<_> = (0..2000)
        .map(|i| RemoteFile {
            id: format!("outside-{i}"),
            name: format!("outside-{i}"),
            parents: vec!["not-this-profile".into()],
            ..Default::default()
        })
        .collect();
    e.db.remote_page(&unrelated, "test-unrelated", "complete")
        .await
        .unwrap();
    let plan = sqlx::query(&format!(
        "EXPLAIN QUERY PLAN {}",
        mygdrive_sync::offline::SCOPED_METADATA_SQL
    ))
    .bind("root")
    .fetch_all(&e.db.pool)
    .await
    .unwrap();
    let indexed_searches = plan
        .iter()
        .filter(|row| {
            let detail: String = row.get(3);
            detail.contains("SEARCH") && detail.contains("offline_remote_parent")
        })
        .count();
    assert_eq!(
        indexed_searches, 2,
        "Both anchor and recursive expansion must use parent lookup"
    );
    let page = e.offline_page("root", 0).await.unwrap();
    assert_eq!(page.children.len(), 1);
    assert_eq!(page.current.counts.total, 1);
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn offline_browser_skips_unchanged_idle_inventory_and_explicit_reload_has_new_revision() {
    use mygdrive_sync::{
        offline_controller::{self, Command, Event, Job},
        service::{self, Action, Request},
    };
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"one");
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    let state = f.state.clone();
    let task = tokio::spawn(service::run_owned(
        e.clone(),
        state.clone(),
        service_config(&f),
        true,
        CancellationToken::new(),
    ));
    service_wait(&state, |s| s.paused).await;
    let controller = offline_controller::spawn(state.clone()).unwrap();
    async fn page(receiver: &std::sync::mpsc::Receiver<Event>) -> u64 {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match receiver.try_recv() {
                    Ok(Event::Page { revision, .. }) => return revision,
                    Ok(Event::Error { message, .. }) => panic!("{message}"),
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        })
        .await
        .unwrap()
    }
    assert_eq!(page(&controller.events).await, 0);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(controller.events.try_recv().is_err());
    controller
        .commands
        .try_send(Job {
            revision: 1,
            command: Command::List {
                parent: "root".into(),
                offset: 0,
            },
        })
        .unwrap();
    assert_eq!(page(&controller.events).await, 1);
    drop(controller);
    service::send(&state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn ondemand_hydration_is_scoped_verified_and_does_not_pin_or_overwrite_edits() {
    let f = Fixture::new(true);
    f.drive.put("one", "online.txt", b"remote content");
    f.drive.put("outside", "outside.txt", b"outside");
    f.drive
        .files
        .lock()
        .unwrap()
        .get_mut("outside")
        .unwrap()
        .0
        .parents = vec!["elsewhere".into()];
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    let one = e.db.by_remote("one").await.unwrap().unwrap();
    let view = mygdrive_sync::cloud_view::View::load(&e.db).await.unwrap();
    assert_eq!(view.target("online.txt").unwrap().size, 14);
    assert!(view.target("outside.txt").is_err());
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 0);
    assert!(!f.root.join("online.txt").exists());
    f.drive.offline.store(true, Ordering::SeqCst);
    assert!(e.hydrate(&one.id).await.is_err());
    assert!(!f.root.join("online.txt").exists());
    f.drive.offline.store(false, Ordering::SeqCst);
    e.hydrate(&one.id).await.unwrap();
    assert_eq!(
        std::fs::read(f.root.join("online.txt")).unwrap(),
        b"remote content"
    );
    assert!(!e.offline_page("root", 0).await.unwrap().children[0].effective);
    std::fs::write(f.root.join("online.txt"), b"offline edit").unwrap();
    e.hydrate(&one.id).await.unwrap();
    assert_eq!(
        std::fs::read(f.root.join("online.txt")).unwrap(),
        b"offline edit"
    );
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 1);
    assert!(e.hydrate("nonexistent").await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ondemand_open_bypasses_blocked_background_download_and_coalesces_same_file() {
    let f = Fixture::new(true);
    f.drive.put("slow", "slow.txt", b"background bytes");
    f.drive.put("fast", "fast.txt", b"foreground bytes");
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    let slow = e.db.by_remote("slow").await.unwrap().unwrap();
    let fast = e.db.by_remote("fast").await.unwrap().unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:slow", true)
        .await
        .unwrap();
    f.drive.hold_download.store(true, Ordering::SeqCst);
    let engine = e.clone();
    let background = tokio::spawn(async move { engine.sync_once().await });
    tokio::time::timeout(Duration::from_secs(5), f.drive.download_entered.notified())
        .await
        .unwrap();
    // Same-file requests wait for the background owner, while another file can
    // download through the shared cache without waiting for that owner.
    let mut readers = Vec::new();
    for _ in 0..6 {
        let engine = e.clone();
        let id = slow.id.clone();
        readers.push(tokio::spawn(async move { engine.hydrate(&id).await }));
    }
    tokio::time::timeout(Duration::from_secs(3), e.hydrate(&fast.id))
        .await
        .expect("explicit open waited for unrelated background transfer")
        .unwrap();
    assert!(!background.is_finished());
    assert_eq!(
        std::fs::read(f.root.join("fast.txt")).unwrap(),
        b"foreground bytes"
    );
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 2);
    f.drive.release_download.notify_one();
    assert!(background.await.unwrap().unwrap().errors.is_empty());
    for reader in readers {
        reader.await.unwrap().unwrap();
    }
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 2);
    assert!(e.db.operations().await.unwrap().is_empty());
    assert_eq!(e.db.file(&fast.id).await.unwrap().status, "synced");
    e.sync_once().await.unwrap();
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ondemand_slots_are_bounded_and_available_local_bytes_do_not_queue() {
    let f = Fixture::new(true);
    for id in ["one", "two", "three", "cached"] {
        f.drive.put(id, &format!("{id}.txt"), id.as_bytes());
    }
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    let cached = e.db.by_remote("cached").await.unwrap().unwrap();
    e.hydrate(&cached.id).await.unwrap();
    let mut tasks = Vec::new();
    for remote in ["one", "two"] {
        let id = e.db.by_remote(remote).await.unwrap().unwrap().id;
        f.drive.hold_download.store(true, Ordering::SeqCst);
        let engine = e.clone();
        tasks.push(tokio::spawn(async move { engine.hydrate(&id).await }));
        tokio::time::timeout(Duration::from_secs(5), f.drive.download_entered.notified())
            .await
            .unwrap();
    }
    let id = e.db.by_remote("three").await.unwrap().unwrap().id;
    let engine = e.clone();
    let third = tokio::spawn(async move { engine.hydrate(&id).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!third.is_finished());
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 3);
    std::fs::write(f.root.join("cached.txt"), b"local unsaved-to-cloud edit").unwrap();
    tokio::time::timeout(Duration::from_secs(1), e.hydrate(&cached.id))
        .await
        .expect("available bytes queued behind foreground slots")
        .unwrap();
    f.drive.release_download.notify_waiters();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    third.await.unwrap().unwrap();
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        std::fs::read(f.root.join("cached.txt")).unwrap(),
        b"local unsaved-to-cloud edit"
    );
    assert!(e.db.operations().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ondemand_metadata_delete_during_download_does_not_resurrect_file() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"remote bytes");
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    let file = e.db.by_remote("one").await.unwrap().unwrap();
    f.drive.hold_download.store(true, Ordering::SeqCst);
    let engine = e.clone();
    let id = file.id.clone();
    let reader = tokio::spawn(async move { engine.hydrate(&id).await });
    tokio::time::timeout(Duration::from_secs(5), f.drive.download_entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        e.virtual_metadata(&uuid::Uuid::new_v4().to_string(), &file.id, "one.txt", None),
    )
    .await
    .expect("metadata intent waited for download")
    .unwrap();
    f.drive.release_download.notify_one();
    assert!(reader.await.unwrap().is_err());
    assert!(!f.root.join("one.txt").exists());
    assert_eq!(e.db.file(&file.id).await.unwrap().status, "deleted_local");
    e.sync_once().await.unwrap();
    assert!(!f.root.join("one.txt").exists());
    assert!(f.drive.files.lock().unwrap()["one"].0.trashed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ondemand_retries_partial_failure_using_existing_journal() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"complete original content");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    let file = e.db.by_remote("one").await.unwrap().unwrap();
    f.drive
        .fail_download_midstream
        .store(true, Ordering::SeqCst);
    assert!(e.hydrate(&file.id).await.is_err());
    assert!(!f.root.join("one.txt").exists());
    let pending = e.db.operations().await.unwrap();
    assert_eq!(pending.len(), 1);
    e.hydrate(&file.id).await.unwrap();
    assert_eq!(
        std::fs::read(f.root.join("one.txt")).unwrap(),
        b"complete original content"
    );
    assert!(e.db.operations().await.unwrap().is_empty());
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ondemand_install_rejects_backing_root_replaced_during_transfer() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"verified original");
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    let id = e.db.by_remote("one").await.unwrap().unwrap().id;
    f.drive.hold_download.store(true, Ordering::SeqCst);
    let engine = e.clone();
    let target = id.clone();
    let reader = tokio::spawn(async move { engine.hydrate(&target).await });
    tokio::time::timeout(Duration::from_secs(5), f.drive.download_entered.notified())
        .await
        .unwrap();
    let old_root = f.root.with_extension("original");
    std::fs::rename(&f.root, &old_root).unwrap();
    std::fs::create_dir(&f.root).unwrap();
    f.drive.release_download.notify_one();
    assert!(reader.await.unwrap().is_err());
    assert!(!f.root.join("one.txt").exists());
    assert!(!old_root.join("one.txt").exists());
    assert!(e.hydrate(&id).await.is_err());
    std::fs::remove_dir(&f.root).unwrap();
    std::fs::rename(old_root, &f.root).unwrap();
    e.hydrate(&id).await.unwrap();
    assert_eq!(
        std::fs::read(f.root.join("one.txt")).unwrap(),
        b"verified original"
    );
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_pruning_waits_until_foreground_bytes_are_installed() {
    let f = Fixture::new(true);
    f.drive.put("one", "one.txt", b"foreground original");
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    let id = e.db.by_remote("one").await.unwrap().unwrap().id;
    f.drive.hold_download.store(true, Ordering::SeqCst);
    let engine = e.clone();
    let reader = tokio::spawn(async move { engine.hydrate(&id).await });
    tokio::time::timeout(Duration::from_secs(5), f.drive.download_entered.notified())
        .await
        .unwrap();
    let engine = e.clone();
    let prune = tokio::spawn(async move { engine.prune_cache(0).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!prune.is_finished());
    f.drive.release_download.notify_one();
    tokio::time::timeout(Duration::from_secs(5), reader)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let report = tokio::time::timeout(Duration::from_secs(5), prune)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(report.removed_objects, 1);
    assert_eq!(
        std::fs::read(f.root.join("one.txt")).unwrap(),
        b"foreground original"
    );
    assert!(e.db.operations().await.unwrap().is_empty());
}

#[tokio::test]
async fn thunar_selection_validates_entire_batch_rejects_escape_and_uses_live_service() {
    use mygdrive_sync::{service, thunar};
    let f = Fixture::new(true);
    f.drive.put("one", "space ' & $file.txt", b"one");
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    let mount = f.root.parent().unwrap().join("online");
    std::fs::create_dir(&mount).unwrap();
    let state = f.state.clone();
    let task = tokio::spawn(service::run_owned(
        e.clone(),
        state.clone(),
        service_config(&f),
        true,
        CancellationToken::new(),
    ));
    service_wait(&state, |s| s.paused).await;
    let chosen = mount.join("space ' & $file.txt");
    assert!(thunar::set_paths(
        &state,
        "test",
        Some(&mount),
        &[chosen.clone(), f.root.parent().unwrap().join("outside")],
        true
    )
    .await
    .is_err());
    assert!(!e.offline_page("root", 0).await.unwrap().children[0].effective);
    assert_eq!(
        thunar::set_paths(
            &state,
            "test",
            Some(&mount),
            &[chosen.clone(), chosen.clone()],
            true
        )
        .await
        .unwrap(),
        1
    );
    assert!(e.offline_page("root", 0).await.unwrap().children[0].effective);
    assert!(!f.root.join("space ' & $file.txt").exists());
    assert!(
        mygdrive_sync::cloud_view::relative_selection(&mount, &mount.join("../outside")).is_err()
    );
    let link = f.root.join("escape");
    std::os::unix::fs::symlink("/tmp", &link).unwrap();
    assert!(mygdrive_sync::cloud_view::relative_selection(&f.root, &link).is_err());
    std::fs::remove_file(link).unwrap();
    thunar::set_paths(
        &state,
        "test",
        Some(&mount),
        std::slice::from_ref(&mount),
        true,
    )
    .await
    .unwrap();
    assert!(thunar::set_paths(
        &state,
        "test",
        Some(&mount),
        std::slice::from_ref(&chosen),
        false
    )
    .await
    .is_err());
    thunar::set_paths(
        &state,
        "test",
        Some(&mount),
        std::slice::from_ref(&mount),
        false,
    )
    .await
    .unwrap();
    thunar::set_paths(&state, "test", Some(&mount), &[chosen], false)
        .await
        .unwrap();
    assert!(!e.offline_page("root", 0).await.unwrap().children[0].effective);
    service::send(&state, &service::Request::new(service::Action::Shutdown))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[cfg(feature = "virtual-drive")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fuse_namespace_lists_without_download_and_supports_offline_atomic_editor_writes() {
    use mygdrive_sync::{
        service,
        virtual_drive::{validate_mountpoint, DriveFs},
    };
    if !std::path::Path::new("/dev/fuse").exists() {
        assert_ne!(
            std::env::var("MYGDRIVE_TEST_FUSE").as_deref(),
            Ok("1"),
            "FUSE is required for this verification"
        );
        eprintln!("FUSE unavailable on this test host; set MYGDRIVE_TEST_FUSE=1 to require it");
        return;
    }
    let f = Fixture::new(true);
    f.drive.put("one", "cloud.txt", b"cloud bytes");
    for (id, name) in [
        ("unlink-target", "unlink-target.txt"),
        ("rename-target", "rename-target.txt"),
        ("truncate-target", "truncate-target.txt"),
        ("copy-target", "copy-target.txt"),
    ] {
        f.drive.put(id, name, b"remote content for mutation");
    }
    std::fs::create_dir(f.root.join("other-folder")).unwrap();
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    let mount = f.root.parent().unwrap().join("online-mount");
    std::fs::create_dir(&mount).unwrap();
    assert!(validate_mountpoint(&f.root, &f.root, &f.state).is_err());
    std::fs::write(mount.join("do-not-hide"), b"existing").unwrap();
    assert!(validate_mountpoint(&mount, &f.root, &f.state).is_err());
    std::fs::remove_file(mount.join("do-not-hide")).unwrap();
    let task = tokio::spawn(service::run_owned(
        e.clone(),
        f.state.clone(),
        service_config(&f),
        false,
        CancellationToken::new(),
    ));
    service_wait(&f.state, |s| {
        s.running && !s.cycle_active && s.cycles_finished > 0
    })
    .await;
    let state = f.state.clone();
    let target = mount.clone();
    let session = tokio::task::spawn_blocking(move || {
        let fs = DriveFs::new(&state, "test").unwrap();
        fuser::spawn_mount2(
            fs,
            target,
            &[
                fuser::MountOption::DefaultPermissions,
                fuser::MountOption::NoExec,
                fuser::MountOption::NoSuid,
                fuser::MountOption::NoDev,
            ],
        )
        .unwrap()
    })
    .await
    .unwrap();
    let target = mount.clone();
    let names = tokio::task::spawn_blocking(move || {
        std::fs::read_dir(target)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>()
    })
    .await
    .unwrap();
    assert!(names.contains(&std::ffi::OsString::from("cloud.txt")));
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 0);
    assert!(!f.root.join("cloud.txt").exists());
    // A pending cloud download must not block directory browsing/stat calls.
    f.drive.hold_download.store(true, Ordering::SeqCst);
    let target = mount.join("cloud.txt");
    let read = tokio::task::spawn_blocking(move || std::fs::read(target).unwrap());
    tokio::time::timeout(Duration::from_secs(5), f.drive.download_entered.notified())
        .await
        .unwrap();
    let target = mount.clone();
    let browsing = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            let entries = std::fs::read_dir(&target).unwrap().count();
            let size = std::fs::metadata(target.join("cloud.txt")).unwrap().len();
            (entries, size)
        }),
    )
    .await;
    f.drive.release_download.notify_one();
    assert_eq!(read.await.unwrap(), b"cloud bytes");
    let (entries, size) = browsing
        .expect("browsing blocked behind hydration")
        .unwrap();
    assert!(entries > 0);
    assert_eq!(size, 11);
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 1);
    assert!(!e.offline_page("root", 0).await.unwrap().children[0].effective);
    // The kernel holds parent directory locks during unlink/rename. Queue cloud-only
    // metadata intent locally instead of downloading while those locks are held.
    f.drive.hold_get.store(true, Ordering::SeqCst);
    service::send(&f.state, &service::Request::new(service::Action::SyncNow))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), f.drive.get_entered.notified())
        .await
        .unwrap();
    // Unlike a plain readdir benchmark, Thunar also sniffs file contents while
    // entering a subfolder. With sync stalled, those speculative opens must
    // finish immediately, without hydrating a placeholder or waiting on cycle.
    let target = mount.clone();
    let backing = f.root.clone();
    let sniff = tokio::task::spawn_blocking(move || {
        std::process::Command::new("python3")
            .arg("-c")
            .arg(
                r#"import ctypes,errno,pathlib,sys,os
assert ctypes.CDLL(None).prctl(15,b'Thunar',0,0,0)==0
p=pathlib.Path(sys.argv[1])
assert (p/'cloud.txt').read_bytes()==b'cloud bytes'
try:
    fd=os.open(p/'truncate-target.txt',os.O_RDONLY|os.O_NOATIME)
    try: os.read(fd,16384)
    finally: os.close(fd)
except OSError as e: assert e.errno==errno.EOPNOTSUPP,e
else: raise AssertionError('speculative read hydrated cloud file')
list((p/'other-folder').iterdir())
os.setxattr(p/'other-folder',b'user.mygdrive.emblem',b'mygdrive-online')
assert os.getxattr(p/'other-folder',b'user.mygdrive.emblem')==b'mygdrive-online'
assert 'user.mygdrive.emblem' not in os.listxattr(pathlib.Path(sys.argv[2])/'other-folder')
assert ctypes.CDLL(None).prctl(15,b'tumblerd',0,0,0)==0
assert (p/'cloud.txt').read_bytes()==b'cloud bytes'
try:
    with (p/'truncate-target.txt').open('rb',buffering=0) as stream: stream.read(131072)
except OSError as e: assert e.errno==errno.EOPNOTSUPP,e
else: raise AssertionError('thumbnail read hydrated cloud file')
"#,
            )
            .arg(target)
            .arg(backing)
            .output()
            .unwrap()
    });
    let sniff = tokio::time::timeout(Duration::from_secs(2), sniff).await;
    if sniff.is_err() {
        f.drive.release_get.notify_one();
    }
    let output = sniff
        .expect("Thunar MIME sniff waited behind active sync")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (action, name) in [
        ("unlink", "unlink-target.txt"),
        ("rename", "rename-target.txt"),
    ] {
        let path = mount.join(name);
        let destination = mount.join("renamed-target.txt");
        let operation = tokio::task::spawn_blocking(move || {
            if action == "unlink" {
                std::fs::remove_file(path)
            } else {
                std::fs::rename(path, destination)
            }
        });
        let completed = tokio::time::timeout(Duration::from_secs(2), operation).await;
        if completed.is_err() {
            f.drive.release_get.notify_one();
        }
        completed
            .expect("cloud-only metadata request waited behind network sync")
            .unwrap()
            .unwrap();
        let target = mount.clone();
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || {
                std::fs::read_dir(target.join("other-folder"))
                    .unwrap()
                    .count();
                std::fs::metadata(target.join("cloud.txt")).unwrap();
            }),
        )
        .await
        .expect("browsing blocked behind cloud-only metadata intent")
        .unwrap();
    }
    assert_eq!(
        f.drive.download_calls.load(Ordering::SeqCst),
        1,
        "rename/delete must not hydrate cloud-only files"
    );
    // A real file-manager copy uses bulk reads, rather than a MIME header
    // probe. It must still hydrate through the verified download path.
    let target = mount.join("copy-target.txt");
    let copy = tokio::task::spawn_blocking(move || {
        std::process::Command::new("python3").arg("-c")
            .arg("import ctypes,sys\nassert ctypes.CDLL(None).prctl(15,b'Thunar',0,0,0)==0\nwith open(sys.argv[1],'rb',buffering=0) as f:\n assert f.read(131072)==b'remote content for mutation'\n assert f.read(131072)==b''\n")
            .arg(target).output().unwrap()
    });
    let copy = tokio::time::timeout(Duration::from_secs(3), copy).await;
    if copy.is_err() {
        f.drive.release_get.notify_one();
    }
    let copy = copy
        .expect("actual FUSE open waited for background GET")
        .unwrap();
    assert!(
        copy.status.success(),
        "{}",
        String::from_utf8_lossy(&copy.stderr)
    );
    assert_eq!(
        std::fs::read(f.root.join("copy-target.txt")).unwrap(),
        b"remote content for mutation"
    );
    assert!(
        service::send(&f.state, &service::Request::new(service::Action::Status))
            .await
            .unwrap()
            .status
            .cycle_active,
        "background GET must still be blocked while the foreground copy succeeds"
    );
    f.drive.release_get.notify_one();
    service_wait(&f.state, |s| !s.cycle_active).await;
    // Truncate needs content, but only holds the affected file's inode lock.
    f.drive.hold_download.store(true, Ordering::SeqCst);
    let path = mount.join("truncate-target.txt");
    let mutation = tokio::task::spawn_blocking(move || {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        if unsafe { libc::truncate(path.as_ptr(), 3) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    });
    tokio::time::timeout(Duration::from_secs(5), f.drive.download_entered.notified())
        .await
        .unwrap();
    let target = mount.clone();
    let browsing = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            std::fs::read_dir(target.join("other-folder"))
                .unwrap()
                .count();
            std::fs::metadata(target.join("cloud.txt")).unwrap();
        }),
    )
    .await;
    f.drive.release_download.notify_one();
    mutation.await.unwrap().unwrap();
    browsing
        .expect("another folder was blocked behind truncate hydration")
        .unwrap();
    f.drive.offline.store(true, Ordering::SeqCst);
    service::send(&f.state, &service::Request::new(service::Action::Pause))
        .await
        .unwrap();
    service_wait(&f.state, |s| s.paused && !s.cycle_active).await;
    let target = mount.clone();
    tokio::task::spawn_blocking(move || {
        std::fs::write(target.join("save.tmp"), b"offline atomic edit").unwrap();
        std::fs::rename(target.join("save.tmp"), target.join("cloud.txt")).unwrap();
        assert_eq!(
            std::fs::read(target.join("cloud.txt")).unwrap(),
            b"offline atomic edit"
        );
        std::fs::create_dir(target.join("nested")).unwrap();
        std::fs::write(target.join("nested/new.txt"), b"local new").unwrap();
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(f.root.join("cloud.txt")).unwrap(),
        b"offline atomic edit"
    );
    service_wait(&f.state, |s| s.pending_files >= 2).await;
    tokio::task::spawn_blocking(move || drop(session))
        .await
        .unwrap();
    assert!(std::fs::read_dir(&mount).unwrap().next().is_none());
    service::send(&f.state, &service::Request::new(service::Action::Shutdown))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
    f.drive.offline.store(false, Ordering::SeqCst);
    e.sync_once().await.unwrap();
    assert_eq!(
        f.drive
            .get("one")
            .await
            .unwrap()
            .unwrap()
            .matches(&fingerprint(&f.root.join("cloud.txt")).unwrap().digest),
        Some(true)
    );
}

async fn subtree_for_migration(f: &Fixture) -> SyncEngine {
    put_folder(&f.drive, "test-folder", "MyGDrive-Test", "root");
    f.drive.put("inside", "a.txt", b"baseline");
    move_under(&f.drive, "inside", "test-folder");
    f.drive.put("outside", "online.txt", b"outside bytes");
    let e = SyncEngine::open(&f.root, &f.state, "test", "test-folder", f.drive.clone())
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "root", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    let mut config = service_config(f);
    config.drive_folder = "test-folder".into();
    std::fs::write(
        mygdrive_sync::service::config_path(&f.state),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    e
}

#[tokio::test]
async fn expand_my_drive_keeps_dirty_identity_and_pins_only_old_subtree() {
    use mygdrive_sync::{cloud_view::View, scope_migration as m};
    let f = Fixture::new(true);
    let e = subtree_for_migration(&f).await;
    let id = e.db.by_remote("inside").await.unwrap().unwrap().id;
    std::fs::write(f.root.join("a.txt"), b"offline pending edit").unwrap();
    let original = fingerprint(&f.root.join("a.txt")).unwrap();
    drop(e);
    let remote = f.drive.get("root").await.unwrap().unwrap();
    let db = mygdrive_sync::db::Database::open(&f.state.join("state.sqlite3"), "test")
        .await
        .unwrap();
    sqlx::query("UPDATE sync_cursors SET token='partial' WHERE name='inventory'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(m::preview(&f.state, "test", &remote).await.is_err());
    sqlx::query("UPDATE sync_cursors SET token='complete' WHERE name='inventory'")
        .execute(&db.pool)
        .await
        .unwrap();
    db.pool.close().await;
    let plan = m::begin(&f.state, "test", &remote).await.unwrap();
    assert_eq!(plan.pending_files, 1);
    assert!(
        SyncEngine::open(&f.root, &f.state, "test", "test-folder", f.drive.clone())
            .await
            .is_err()
    );
    m::resume(&f.state, "test").await.unwrap();
    let e = online_engine(&f).await;
    let after = e.db.file(&id).await.unwrap();
    assert_eq!(after.relative_path, "MyGDrive-Test/a.txt");
    assert_eq!(
        after.baseline.as_ref().unwrap().relative_path,
        "MyGDrive-Test/a.txt"
    );
    let migrated = fingerprint(&f.root.join(&after.relative_path)).unwrap();
    assert_eq!(migrated.inode, original.inode);
    assert_eq!(migrated.digest, original.digest);
    assert_eq!(after.status, "modified_local");
    assert_eq!(
        e.db.setting("folder").await.unwrap().as_deref(),
        Some("root")
    );
    let page = e.offline_page("root", 0).await.unwrap();
    assert!(!page.current.effective);
    assert_eq!(page.current.name, "My Drive");
    assert!(
        page.children
            .iter()
            .find(|n| n.key == "r:test-folder")
            .unwrap()
            .effective
    );
    let calls = f.drive.download_calls.load(Ordering::SeqCst);
    e.sync_once().await.unwrap();
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), calls);
    assert!(!f.root.join("online.txt").exists());
    assert!(View::load(&e.db)
        .await
        .unwrap()
        .target("online.txt")
        .is_ok());
    assert_eq!(
        f.drive
            .get("inside")
            .await
            .unwrap()
            .unwrap()
            .matches(&migrated.digest),
        Some(true)
    );
    assert!(
        m::preview(&f.state, "test", &remote)
            .await
            .unwrap()
            .already_my_drive
    );
    assert!(std::fs::read_dir(f.state.join("backups"))
        .unwrap()
        .next()
        .is_some());
}

#[tokio::test]
async fn expand_my_drive_recovers_after_root_rename_without_double_prefix_or_lost_rename_intent() {
    use mygdrive_sync::scope_migration as m;
    let f = Fixture::new(true);
    let e = subtree_for_migration(&f).await;
    std::fs::rename(f.root.join("a.txt"), f.root.join("renamed.txt")).unwrap();
    e.scanner.scan().await.unwrap();
    let id = e.db.by_remote("inside").await.unwrap().unwrap().id;
    drop(e);
    let remote = f.drive.get("root").await.unwrap().unwrap();
    m::begin(&f.state, "test", &remote).await.unwrap();
    let d = mygdrive_sync::db::Database::read_only(&f.state.join("state.sqlite3"), "test")
        .await
        .unwrap();
    let raw = d.setting("scope_migration_journal").await.unwrap().unwrap();
    let j: serde_json::Value = serde_json::from_str(&raw).unwrap();
    d.pool.close().await;
    std::fs::rename(&f.root, j["stage"].as_str().unwrap()).unwrap();
    m::resume(&f.state, "test").await.unwrap();
    let e = online_engine(&f).await;
    let record = e.db.file(&id).await.unwrap();
    assert_eq!(record.relative_path, "MyGDrive-Test/renamed.txt");
    assert_eq!(
        record.baseline.unwrap().relative_path,
        "MyGDrive-Test/a.txt"
    );
    assert_eq!(
        std::fs::read(f.root.join("MyGDrive-Test/renamed.txt")).unwrap(),
        b"baseline"
    );
    assert!(!m::pending(&f.state, "test").await.unwrap());
}

#[tokio::test]
async fn expand_my_drive_recovers_committed_database_and_refuses_running_writer() {
    use mygdrive_sync::{scope_migration as m, service};
    let f = Fixture::new(true);
    let e = subtree_for_migration(&f).await;
    let remote = f.drive.get("root").await.unwrap().unwrap();
    assert!(m::begin(&f.state, "test", &remote).await.is_err());
    drop(e);
    m::begin(&f.state, "test", &remote).await.unwrap();
    let config_path = service::config_path(&f.state);
    std::fs::remove_file(&config_path).unwrap();
    std::fs::create_dir(&config_path).unwrap();
    assert!(m::resume(&f.state, "test").await.is_err());
    assert!(m::pending(&f.state, "test").await.unwrap());
    assert!(
        SyncEngine::open(&f.root, &f.state, "test", "root", f.drive.clone())
            .await
            .is_err()
    );
    std::fs::remove_dir(&config_path).unwrap();
    // CLI recovery must work without a service config, --root or OAuth/network.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mygdrive-sync"))
        .arg("--state")
        .arg(&f.state)
        .args(["--account", "test", "expand-my-drive", "--apply"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let e = online_engine(&f).await;
    assert_eq!(
        e.db.by_remote("inside")
            .await
            .unwrap()
            .unwrap()
            .relative_path,
        "MyGDrive-Test/a.txt"
    );
    assert_eq!(service::load_config(&f.state).unwrap().drive_folder, "root");
}

#[tokio::test]
async fn large_online_inventory_registers_once_without_rewriting_unchanged_file_status() {
    let f = Fixture::new(true);
    for i in 0..2050 {
        f.drive.put(
            &format!("item-{i}"),
            &format!("file-{i}.txt"),
            b"metadata only",
        );
    }
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    assert_eq!(e.db.files().await.unwrap().len(), 2050);
    sqlx::raw_sql("CREATE TABLE status_writes(n INTEGER); INSERT INTO status_writes VALUES(0); CREATE TRIGGER count_status_writes AFTER UPDATE ON files BEGIN UPDATE status_writes SET n=n+1; END;").execute(&e.db.pool).await.unwrap();
    e.sync_once().await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT n FROM status_writes")
        .fetch_one(&e.db.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        mygdrive_sync::cloud_view::View::load(&e.db)
            .await
            .unwrap()
            .entries
            .len(),
        2051
    );
    let query = format!(
        "SELECT data FROM files WHERE {} ORDER BY relative_path",
        mygdrive_sync::db::RECONCILIATION_FILTER
    );
    let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(&format!("EXPLAIN QUERY PLAN {query}"))
        .fetch_all(&e.db.pool)
        .await
        .unwrap();
    assert!(
        plan.iter()
            .any(|row| row.3.contains("reconciliation_candidates")),
        "{plan:?}"
    );
    assert!(sqlx::query_scalar::<_, String>(&query)
        .fetch_all(&e.db.pool)
        .await
        .unwrap()
        .is_empty());
    // An online-only row outside the index must still be included immediately
    // when selected, then remain indexed for subsequent offline edits.
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "r:item-777", true)
        .await
        .unwrap();
    f.drive.get_calls.lock().unwrap().clear();
    e.sync_once().await.unwrap();
    assert_eq!(
        std::fs::read(f.root.join("file-777.txt")).unwrap(),
        b"metadata only"
    );
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 1);
    assert!(f
        .drive
        .get_calls
        .lock()
        .unwrap()
        .iter()
        .all(|id| id == "root" || id == "item-777"));
    assert_eq!(
        sqlx::query_scalar::<_, String>(&query)
            .fetch_all(&e.db.pool)
            .await
            .unwrap()
            .len(),
        1
    );
    std::fs::write(f.root.join("file-777.txt"), b"offline edit").unwrap();
    e.sync_once().await.unwrap();
    assert_eq!(f.drive.data("item-777"), b"offline edit");
}

#[tokio::test]
async fn service_status_remains_responsive_while_metadata_writer_lease_is_held() {
    use mygdrive_sync::service::{self, Action, Request};
    let f = Fixture::new(true);
    let e = Arc::new(online_engine(&f).await);
    e.sync_once().await.unwrap();
    let task = tokio::spawn(service::run_owned(
        e.clone(),
        f.state.clone(),
        service_config(&f),
        true,
        CancellationToken::new(),
    ));
    service_wait(&f.state, |s| s.paused).await;
    let tx = e.db.pool.begin().await.unwrap();
    // Cross at least one heartbeat while the sole writer connection is leased.
    // Neither accepting clients nor non-Status command dedupe may wait for it.
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let status = Request::new(Action::Status);
    let sync_now = Request::new(Action::SyncNow);
    let responses = tokio::time::timeout(Duration::from_millis(500), async {
        tokio::join!(
            service::send(&f.state, &status),
            service::send(&f.state, &sync_now)
        )
    })
    .await;
    tx.rollback().await.unwrap();
    let (status, sync_now) = responses.expect("IPC blocked behind metadata writer/heartbeat");
    let status = status.unwrap();
    assert!(status.accepted);
    assert!(status.status.running);
    let sync_now = sync_now.unwrap();
    assert!(!sync_now.accepted);
    assert_eq!(
        sync_now.error.as_deref(),
        Some("service is paused; resume first")
    );
    service::send(&f.state, &Request::new(Action::Shutdown))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn expand_nested_scope_preserves_colliding_paths_and_rejects_mismatched_config() {
    use mygdrive_sync::{scope_migration as m, service};
    let f = Fixture::new(true);
    put_folder(&f.drive, "a", "A", "root");
    put_folder(&f.drive, "b", "B", "a");
    put_folder(&f.drive, "test-folder", "MyGDrive-Test", "b");
    f.drive.put("inside", "a.txt", b"baseline");
    move_under(&f.drive, "inside", "test-folder");
    let e = SyncEngine::open(&f.root, &f.state, "test", "test-folder", f.drive.clone())
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "root", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    let old_inode = fingerprint(&f.root.join("a.txt")).unwrap().inode;
    let prefix = "A/B/MyGDrive-Test";
    std::fs::create_dir_all(f.root.join(prefix)).unwrap();
    std::fs::write(f.root.join(prefix).join("a.txt"), b"different local file").unwrap();
    e.scanner.scan().await.unwrap();
    let mut config = service_config(&f);
    config.drive_folder = "test-folder".into();
    let wrong = f.root.parent().unwrap().join("other-root");
    std::fs::create_dir(&wrong).unwrap();
    config.root = wrong.clone();
    std::fs::write(
        service::config_path(&f.state),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    drop(e);
    let remote = f.drive.get("root").await.unwrap().unwrap();
    assert!(m::begin(&f.state, "test", &remote).await.is_err());
    assert!(std::fs::read_dir(&wrong).unwrap().next().is_none());
    assert!(!m::pending(&f.state, "test").await.unwrap());
    config.root = f.root.clone();
    std::fs::write(
        service::config_path(&f.state),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let plan = m::begin(&f.state, "test", &remote).await.unwrap();
    assert_eq!(plan.local_prefix, prefix);
    m::resume(&f.state, "test").await.unwrap();
    let e = online_engine(&f).await;
    let original = e.db.by_remote("inside").await.unwrap().unwrap();
    assert_eq!(original.relative_path, format!("{prefix}/a.txt"));
    assert_eq!(
        fingerprint(&f.root.join(&original.relative_path))
            .unwrap()
            .inode,
        old_inode
    );
    assert_eq!(
        std::fs::read(f.root.join(format!("{prefix}/{prefix}/a.txt"))).unwrap(),
        b"different local file"
    );
    assert_eq!(e.db.files().await.unwrap().len(), 2);
}

#[tokio::test]
async fn expand_nested_scope_recovers_renamed_target_and_rejects_mismatched_config() {
    use mygdrive_sync::{scope_migration as m, service};
    let f = Fixture::new(true);
    put_folder(&f.drive, "a", "A", "root");
    put_folder(&f.drive, "b", "B", "a");
    put_folder(&f.drive, "test-folder", "MyGDrive-Test", "b");
    f.drive.put("inside", "a.txt", b"baseline");
    move_under(&f.drive, "inside", "test-folder");
    let e = SyncEngine::open(&f.root, &f.state, "test", "test-folder", f.drive.clone())
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    e.offline_select(&uuid::Uuid::new_v4().to_string(), "root", true)
        .await
        .unwrap();
    e.sync_once().await.unwrap();
    let old_inode = fingerprint(&f.root.join("a.txt")).unwrap().inode;
    let prefix = "A/B/MyGDrive-Test";
    std::fs::create_dir_all(f.root.join(prefix)).unwrap();
    std::fs::write(f.root.join(prefix).join("a.txt"), b"different local file").unwrap();
    e.scanner.scan().await.unwrap();
    let mut config = service_config(&f);
    config.drive_folder = "test-folder".into();
    let wrong = f.root.parent().unwrap().join("other-root");
    std::fs::create_dir(&wrong).unwrap();
    config.root = wrong.clone();
    std::fs::write(
        service::config_path(&f.state),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    drop(e);
    let remote = f.drive.get("root").await.unwrap().unwrap();
    assert!(m::begin(&f.state, "test", &remote).await.is_err());
    assert!(std::fs::read_dir(&wrong).unwrap().next().is_none());
    assert!(!m::pending(&f.state, "test").await.unwrap());
    config.root = f.root.clone();
    std::fs::write(
        service::config_path(&f.state),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let plan = m::begin(&f.state, "test", &remote).await.unwrap();
    assert_eq!(plan.local_prefix, prefix);
    let d = mygdrive_sync::db::Database::read_only(&f.state.join("state.sqlite3"), "test")
        .await
        .unwrap();
    let j: serde_json::Value =
        serde_json::from_str(&d.setting("scope_migration_journal").await.unwrap().unwrap())
            .unwrap();
    d.pool.close().await;
    // Simulate process loss just after target rename but before parent fsync/DB commit.
    let stage = std::path::PathBuf::from(j["stage"].as_str().unwrap());
    std::fs::rename(&f.root, &stage).unwrap();
    std::fs::create_dir(&f.root).unwrap();
    let target = f.root.join(prefix);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::rename(stage, &target).unwrap();
    m::resume(&f.state, "test").await.unwrap();
    let e = online_engine(&f).await;
    let original = e.db.by_remote("inside").await.unwrap().unwrap();
    assert_eq!(original.relative_path, format!("{prefix}/a.txt"));
    assert_eq!(
        fingerprint(&f.root.join(&original.relative_path))
            .unwrap()
            .inode,
        old_inode
    );
    assert_eq!(
        std::fs::read(f.root.join(format!("{prefix}/{prefix}/a.txt"))).unwrap(),
        b"different local file"
    );
    assert_eq!(e.db.files().await.unwrap().len(), 2);
}

#[tokio::test]
async fn disconnected_download_keeps_original_and_retries_after_restart_with_activity_list() {
    let f = Fixture::new(true);
    let engine = f.synced().await;
    std::fs::write(
        mygdrive_sync::service::config_path(&f.state),
        serde_json::to_vec(&service_config(&f)).unwrap(),
    )
    .unwrap();
    f.drive
        .put("original", "note.txt", b"server content after reconnect");
    f.drive
        .fail_download_midstream
        .store(true, Ordering::SeqCst);
    let report = engine.sync_once().await.unwrap();
    assert_eq!(report.errors.len(), 1);
    assert_eq!(std::fs::read(f.root.join("note.txt")).unwrap(), b"baseline");
    assert_eq!(engine.progress().active_operations, 0);
    assert_eq!(
        std::fs::read_dir(f.state.join("staging")).unwrap().count(),
        0,
        "failed partial download must be cleaned"
    );
    let items = mygdrive_sync::dashboard::read_activity(&f.state)
        .await
        .unwrap();
    assert!(items
        .iter()
        .any(|i| i.path == "note.txt" && i.pending && i.retrying));
    for mut op in engine.db.operations().await.unwrap() {
        op.next_retry_ms = 0;
        engine.db.save_operation(&op).await.unwrap();
    }
    drop(engine);
    let engine = f.engine().await;
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        std::fs::read(f.root.join("note.txt")).unwrap(),
        b"server content after reconnect"
    );
    assert_eq!(
        engine
            .db
            .by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .status,
        "synced"
    );
    let items = mygdrive_sync::dashboard::read_activity(&f.state)
        .await
        .unwrap();
    assert!(items.iter().any(|i| i.path == "note.txt"
        && !i.pending
        && i.detail == mygdrive_sync::i18n::tr("Selesai")));
}

#[cfg(feature = "desktop-ui")]
#[tokio::test]
async fn tray_health_distinguishes_lost_ui_connection_from_confirmed_sync_errors() {
    use mygdrive_sync::{
        connectivity::ConnectionState,
        dashboard::{Notice, Snapshot},
        service,
        tray::Model,
    };
    let f = Fixture::new(true);
    let engine = Arc::new(f.engine().await);
    let stop = CancellationToken::new();
    let task = tokio::spawn(service::run_owned(
        engine,
        f.state.clone(),
        service_config(&f),
        true,
        stop.clone(),
    ));
    let status = service_wait(&f.state, |s| s.paused).await;
    let mut snapshot = Snapshot {
        live: true,
        status: Some(status),
        config: None,
        account: None,
    };
    {
        let s = snapshot.status.as_mut().unwrap();
        s.paused = false;
        s.connection = ConnectionState::Online;
        s.last_error = None;
        s.last_success_ms = Some(1);
    }
    assert!(!Model::from_snapshot(&snapshot, false).attention);
    assert!(!Model::from_snapshot(&snapshot, false).warning);
    snapshot.live = false;
    let model = Model::from_snapshot(&snapshot, false);
    assert!(
        !model.attention,
        "a failed UI poll is not a confirmed sync error"
    );
    assert!(model.warning);
    assert_eq!(snapshot.notice(), Some(Notice::Reconnecting));
    snapshot.live = true;
    {
        let s = snapshot.status.as_mut().unwrap();
        s.connection = ConnectionState::Offline;
        s.last_error = Some("offline".into());
    }
    let model = Model::from_snapshot(&snapshot, false);
    assert!(model.warning && !model.attention);
    assert_eq!(snapshot.notice(), Some(Notice::Offline));
    assert!(!snapshot.notice().unwrap().description().is_empty());
    {
        let s = snapshot.status.as_mut().unwrap();
        s.connection = ConnectionState::AuthRequired;
        s.last_error = Some("auth_required".into());
    }
    assert!(Model::from_snapshot(&snapshot, false).attention);
    assert_eq!(snapshot.notice(), Some(Notice::AuthRequired));
    assert!(!snapshot.notice().unwrap().description().is_empty());
    let login_title = mygdrive_sync::i18n::tr("Sinkronisasi menunggu login ulang Google");
    assert_eq!(snapshot.title(), login_title);
    snapshot.status.as_mut().unwrap().paused = true;
    assert_eq!(
        snapshot.title(),
        login_title,
        "pause must not hide the sign-in requirement"
    );
    snapshot.status.as_mut().unwrap().paused = false;
    snapshot.status.as_mut().unwrap().connection = ConnectionState::Online;
    assert_eq!(
        snapshot.title(),
        login_title,
        "the stored auth error must still be actionable"
    );
    {
        let s = snapshot.status.as_mut().unwrap();
        s.connection = ConnectionState::Online;
        s.last_error = Some("file operation failed".into());
    }
    assert!(Model::from_snapshot(&snapshot, false).attention);
    assert_eq!(snapshot.notice(), Some(Notice::Error));
    assert!(!snapshot.notice().unwrap().description().is_empty());
    snapshot.status.as_mut().unwrap().last_error = None;
    assert_ne!(snapshot.title(), login_title);
    let model = Model::from_snapshot(&snapshot, false);
    assert!(!model.attention && !model.warning && !model.syncing);
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn cloud_only_metadata_intents_are_durable_idempotent_and_do_not_download_content() {
    let f = Fixture::new(true);
    f.drive.put("meta", "online.txt", b"cloud-only content");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    let file = e.db.by_remote("meta").await.unwrap().unwrap();
    let request = uuid::Uuid::new_v4().to_string();
    assert!(!e
        .virtual_metadata(&request, &file.id, "online.txt", Some("first.txt"))
        .await
        .unwrap());
    assert!(e
        .virtual_metadata(&request, &file.id, "online.txt", Some("first.txt"))
        .await
        .unwrap());
    assert!(e
        .virtual_metadata(&request, &file.id, "online.txt", None)
        .await
        .is_err());
    e.virtual_metadata(
        &uuid::Uuid::new_v4().to_string(),
        &file.id,
        "first.txt",
        Some("second.txt"),
    )
    .await
    .unwrap();
    drop(e);
    let e = online_engine(&f).await;
    let report = e.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        f.drive.get("meta").await.unwrap().unwrap().name,
        "second.txt"
    );
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 0);
    assert!(!f.root.join("second.txt").exists());
    e.virtual_metadata(
        &uuid::Uuid::new_v4().to_string(),
        &file.id,
        "second.txt",
        None,
    )
    .await
    .unwrap();
    let report = e.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(f.drive.get("meta").await.unwrap().unwrap().trashed);
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cloud_only_metadata_intents_preserve_server_edits_for_review() {
    let f = Fixture::new(true);
    f.drive.put("meta", "online.txt", b"original cloud content");
    let e = online_engine(&f).await;
    e.sync_once().await.unwrap();
    let file = e.db.by_remote("meta").await.unwrap().unwrap();
    e.virtual_metadata(
        &uuid::Uuid::new_v4().to_string(),
        &file.id,
        "online.txt",
        Some("renamed.txt"),
    )
    .await
    .unwrap();
    f.drive.put(
        "meta",
        "online.txt",
        b"edited on server after rename was queued",
    );
    let report = e.sync_once().await.unwrap();
    assert!(report.deferred > 0);
    assert_eq!(
        e.db.file(&file.id).await.unwrap().status,
        "metadata_conflict_review"
    );
    assert_eq!(
        f.drive.get("meta").await.unwrap().unwrap().name,
        "online.txt"
    );
    assert_eq!(
        f.drive.data("meta"),
        b"edited on server after rename was queued"
    );
    assert_eq!(f.drive.download_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn activity_totals_include_bulk_deletes_before_planning_and_are_not_list_limits() {
    let f = Fixture::new(true);
    for n in 0..45 {
        f.drive.put(
            &format!("bulk-{n}"),
            &format!("bulk-{n:02}.txt"),
            b"content",
        );
    }
    let e = f.engine().await;
    e.sync_once().await.unwrap();
    std::fs::write(
        mygdrive_sync::service::config_path(&f.state),
        serde_json::to_vec(&service_config(&f)).unwrap(),
    )
    .unwrap();
    for n in 0..45 {
        std::fs::remove_file(f.root.join(format!("bulk-{n:02}.txt"))).unwrap();
    }
    e.scanner.scan().await.unwrap();
    let page = mygdrive_sync::dashboard::read_activity_page(&f.state)
        .await
        .unwrap();
    assert_eq!(page.pending_files, 45);
    assert_eq!(page.pending_deletions, 45);
    assert_eq!(page.items.iter().filter(|i| i.pending).count(), 30);
    let file = e.db.by_remote("bulk-0").await.unwrap().unwrap();
    let remote = f.drive.get("bulk-0").await.unwrap();
    e.db.operation(&Operation::new(&file, "trash", remote))
        .await
        .unwrap();
    let page = mygdrive_sync::dashboard::read_activity_page(&f.state)
        .await
        .unwrap();
    assert_eq!(
        page.pending_files, 45,
        "a planned operation must not double-count the same file"
    );
    assert_eq!(page.pending_deletions, 45);
    let paths: std::collections::HashSet<_> = page
        .items
        .iter()
        .filter(|i| i.pending)
        .map(|i| &i.path)
        .collect();
    assert_eq!(paths.len(), 30);
}

#[tokio::test]
async fn scanner_reuses_stable_fingerprints_but_detects_edits_with_restored_mtime() {
    let f = Fixture::new(true);
    let e = f.synced().await;
    let file = e.db.by_remote("original").await.unwrap().unwrap();
    let before = file.fingerprint.unwrap();
    e.scanner.scan().await.unwrap();
    assert_eq!(
        e.db.by_remote("original")
            .await
            .unwrap()
            .unwrap()
            .fingerprint,
        Some(before.clone())
    );
    let path = f.root.join("note.txt");
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(5));
    std::fs::write(&path, b"modified").unwrap(); // same length as baseline
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    e.scanner.scan().await.unwrap();
    let after = e.db.by_remote("original").await.unwrap().unwrap();
    assert_eq!(after.status, "modified_local");
    let after = after.fingerprint.unwrap();
    assert_eq!(after.modified_ns, before.modified_ns);
    assert_ne!(after.changed_ns, before.changed_ns);
    assert_ne!(after.digest, before.digest);
}

#[tokio::test]
async fn file_permission_failure_does_not_block_other_pins_and_recovers_after_restart() {
    use mygdrive_sync::{dashboard, diagnostics::FileIssue, service::ServiceConfig};
    let fixture = Fixture::new(true);
    fixture.drive.put("blocked", "a.txt", b"restricted");
    fixture.drive.put("healthy", "z.txt", b"available");
    let engine = SyncEngine::open(
        &fixture.root,
        &fixture.state,
        "test",
        "root",
        fixture.drive.clone(),
    )
    .await
    .unwrap();
    engine.sync_once().await.unwrap();
    for remote in ["blocked", "healthy"] {
        let file = engine.db.by_remote(remote).await.unwrap().unwrap();
        engine.set_available_offline(&file.id, true).await.unwrap();
    }
    *fixture.drive.get_failure.lock().unwrap() =
        Some(("blocked".into(), 403, "insufficientFilePermissions".into()));
    let report = engine.sync_once().await.unwrap();
    assert_eq!(report.errors.len(), 1);
    assert_eq!(
        std::fs::read(fixture.root.join("z.txt")).unwrap(),
        b"available"
    );
    assert!(!fixture.root.join("a.txt").exists());
    assert_eq!(
        engine
            .db
            .by_remote("blocked")
            .await
            .unwrap()
            .unwrap()
            .metadata_issue,
        Some(FileIssue::Permission)
    );
    std::fs::write(
        mygdrive_sync::service::config_path(&fixture.state),
        serde_json::to_vec(&ServiceConfig {
            version: 1,
            account: "test".into(),
            root: fixture.root.clone(),
            drive_folder: "root".into(),
            interval_seconds: 10,
            cache_max_bytes: None,
        })
        .unwrap(),
    )
    .unwrap();
    let page = dashboard::read_activity_page(&fixture.state).await.unwrap();
    assert_eq!(page.pending_files, 1);
    assert_eq!(page.retry_operations, 1);
    assert!(page.items.iter().any(|i| i.path == "a.txt"
        && i.retrying
        && i.detail == FileIssue::Permission.description()));
    let blocked = engine.db.by_remote("blocked").await.unwrap().unwrap();
    engine
        .set_available_offline(&blocked.id, false)
        .await
        .unwrap();
    assert!(engine.sync_once().await.unwrap().errors.is_empty());
    assert!(engine
        .db
        .file(&blocked.id)
        .await
        .unwrap()
        .metadata_issue
        .is_none());
    assert_eq!(
        dashboard::read_activity_page(&fixture.state)
            .await
            .unwrap()
            .pending_files,
        0
    );
    engine
        .set_available_offline(&blocked.id, true)
        .await
        .unwrap();
    assert_eq!(engine.sync_once().await.unwrap().errors.len(), 1);
    drop(engine);
    *fixture.drive.get_failure.lock().unwrap() = None;
    let engine = SyncEngine::open(
        &fixture.root,
        &fixture.state,
        "test",
        "root",
        fixture.drive.clone(),
    )
    .await
    .unwrap();
    assert!(engine.sync_once().await.unwrap().errors.is_empty());
    assert_eq!(
        std::fs::read(fixture.root.join("a.txt")).unwrap(),
        b"restricted"
    );
    assert!(engine
        .db
        .by_remote("blocked")
        .await
        .unwrap()
        .unwrap()
        .metadata_issue
        .is_none());
    assert_eq!(
        dashboard::read_activity_page(&fixture.state)
            .await
            .unwrap()
            .pending_files,
        0
    );
}

#[tokio::test]
async fn metadata_rate_limit_still_stops_the_account_cycle() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    *fixture.drive.get_failure.lock().unwrap() =
        Some(("original".into(), 403, "rateLimitExceeded".into()));
    assert!(
        matches!(engine.sync_once().await, Err(Error::Http { status: 403, reason }) if reason == "rateLimitExceeded")
    );
    assert!(engine
        .db
        .by_remote("original")
        .await
        .unwrap()
        .unwrap()
        .metadata_issue
        .is_none());
}

#[tokio::test]
async fn rejected_cursor_removes_missing_online_entries_without_deleting_local_edits() {
    let fixture = Fixture::new(true);
    fixture.drive.put("gone", "gone.txt", b"old remote");
    fixture.drive.put("edited", "edited.txt", b"baseline");
    let engine = SyncEngine::open(
        &fixture.root,
        &fixture.state,
        "test",
        "root",
        fixture.drive.clone(),
    )
    .await
    .unwrap();
    engine.sync_once().await.unwrap();
    let edited = engine.db.by_remote("edited").await.unwrap().unwrap();
    engine
        .set_available_offline(&edited.id, true)
        .await
        .unwrap();
    engine.sync_once().await.unwrap();
    std::fs::write(fixture.root.join("edited.txt"), b"local unsent changes").unwrap();
    fixture.drive.files.lock().unwrap().remove("gone");
    fixture.drive.files.lock().unwrap().remove("edited");
    fixture
        .drive
        .reject_cursor_once
        .store(true, Ordering::SeqCst);
    engine.sync_once().await.unwrap();
    let view = mygdrive_sync::cloud_view::View::load(&engine.db)
        .await
        .unwrap();
    assert!(!view.entries.contains_key("gone.txt"));
    assert!(view.entries.contains_key("edited.txt"));
    assert_eq!(
        std::fs::read(fixture.root.join("edited.txt")).unwrap(),
        b"local unsent changes"
    );
    assert!(engine
        .db
        .remotes()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.id != "root")
        .all(|r| r.unavailable));
    assert_eq!(fixture.drive.files.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn interrupted_inventory_retains_old_namespace_until_complete_after_restart() {
    let fixture = Fixture::new(true);
    fixture.drive.put("gone", "gone.txt", b"old");
    fixture.drive.put("kept", "kept.txt", b"keep");
    let engine = SyncEngine::open(
        &fixture.root,
        &fixture.state,
        "test",
        "root",
        fixture.drive.clone(),
    )
    .await
    .unwrap();
    engine.sync_once().await.unwrap();
    engine.db.reset_inventory().await.unwrap();
    let kept = fixture.drive.get("kept").await.unwrap().unwrap();
    engine
        .db
        .remote_page(&[kept], "inventory", "page-two")
        .await
        .unwrap();
    assert!(mygdrive_sync::cloud_view::View::load(&engine.db)
        .await
        .unwrap()
        .entries
        .contains_key("gone.txt"));
    drop(engine);
    let engine = SyncEngine::open(
        &fixture.root,
        &fixture.state,
        "test",
        "root",
        fixture.drive.clone(),
    )
    .await
    .unwrap();
    engine
        .db
        .remote_page(&[], "inventory", "complete")
        .await
        .unwrap();
    let view = mygdrive_sync::cloud_view::View::load(&engine.db)
        .await
        .unwrap();
    assert!(!view.entries.contains_key("gone.txt"));
    assert!(view.entries.contains_key("kept.txt"));
    assert!(
        !engine
            .db
            .remotes()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == "root")
            .unwrap()
            .unavailable
    );
}

#[tokio::test]
async fn completed_uploads_release_snapshots_and_keep_cloud_and_activity_history() {
    let fixture = Fixture::new(true);
    let engine = fixture.engine().await;
    for bytes in [b"version one".as_slice(), b"version two", b"version three"] {
        std::fs::write(fixture.root.join("report.txt"), bytes).unwrap();
        assert!(engine.sync_once().await.unwrap().errors.is_empty());
        assert!(engine.db.operations().await.unwrap().is_empty());
        assert_eq!(
            std::fs::read_dir(fixture.state.join("outbox"))
                .unwrap()
                .count(),
            0
        );
    }
    let file = engine.db.files().await.unwrap().pop().unwrap();
    assert_eq!(
        fixture.drive.data(file.remote_id.as_ref().unwrap()),
        b"version three"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sync_operations WHERE stage='done'")
        .fetch_one(&engine.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn upload_cleanup_preserves_active_cancelled_review_and_conflict_snapshots() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let file = engine.db.by_remote("original").await.unwrap().unwrap();
    let mut paths = Vec::new();
    for stage in ["snapshot_ready", "cancelled"] {
        let snapshot = engine
            .cache
            .snapshot(&fixture.root.join("note.txt"))
            .await
            .unwrap();
        paths.push(snapshot.path.clone());
        let mut op = Operation::new(
            &file,
            "upload",
            file.baseline.as_ref().map(|b| b.remote.clone()),
        );
        op.key = format!("cleanup-protected:{}", op.id);
        op.stage = stage.into();
        op.snapshot = Some(snapshot);
        engine.db.operation(&op).await.unwrap();
    }
    // A completed job can share a blob with an active journal: protect it too.
    let mut shared = Operation::new(&file, "upload", None);
    shared.key = format!("cleanup-shared:{}", shared.id);
    shared.stage = "done".into();
    shared.snapshot = Some(Snapshot {
        path: paths[0].clone(),
        digest: file.fingerprint.as_ref().unwrap().digest.clone(),
    });
    engine.db.operation(&shared).await.unwrap();
    let orphan = engine
        .cache
        .snapshot(&fixture.root.join("note.txt"))
        .await
        .unwrap();
    engine.prune_cache(0).await.unwrap();
    for path in &paths {
        assert!(path.exists());
    }
    assert!(orphan.path.exists());
    let conflict_snapshot = engine
        .cache
        .snapshot(&fixture.root.join("note.txt"))
        .await
        .unwrap();
    let mut conflict = Operation::new(&file, "upload", None);
    conflict.key = format!("cleanup-conflict:{}", conflict.id);
    conflict.stage = "done".into();
    conflict.snapshot = Some(conflict_snapshot.clone());
    engine
        .db
        .conflict(&conflict, "preserved-conflict.txt")
        .await
        .unwrap();
    engine.db.operation(&conflict).await.unwrap();
    engine.prune_cache(0).await.unwrap();
    assert!(conflict_snapshot.path.exists());
    // Explicit review protects completed snapshots even without a conflict row.
    let mut review = file;
    review.review_required = true;
    review.status = "recovery_review".into();
    engine.db.save_file(&review).await.unwrap();
    let snapshot = engine
        .cache
        .snapshot(&fixture.root.join("note.txt"))
        .await
        .unwrap();
    let mut op = Operation::new(&review, "create", None);
    op.key = format!("cleanup-review:{}", op.id);
    op.stage = "done".into();
    op.snapshot = Some(snapshot.clone());
    engine.db.operation(&op).await.unwrap();
    engine.prune_cache(0).await.unwrap();
    assert!(snapshot.path.exists());
}

#[tokio::test]
async fn upload_cleanup_recovers_missing_completed_blob_and_rejects_external_path() {
    let fixture = Fixture::new(true);
    let engine = fixture.synced().await;
    let file = engine.db.by_remote("original").await.unwrap().unwrap();
    let snapshot = engine
        .cache
        .snapshot(&fixture.root.join("note.txt"))
        .await
        .unwrap();
    let mut op = Operation::new(&file, "upload", None);
    op.key = format!("cleanup-interrupted:{}", op.id);
    op.stage = "done".into();
    op.snapshot = Some(snapshot.clone());
    engine.db.operation(&op).await.unwrap();
    std::fs::remove_file(&snapshot.path).unwrap();
    engine.prune_cache(0).await.unwrap();
    let saved: String = sqlx::query_scalar("SELECT data FROM sync_operations WHERE id=?")
        .bind(&op.id)
        .fetch_one(&engine.db.pool)
        .await
        .unwrap();
    assert!(serde_json::from_str::<Operation>(&saved)
        .unwrap()
        .snapshot
        .is_none());
    op.snapshot = Some(Snapshot {
        path: fixture.root.join("note.txt"),
        digest: snapshot.digest,
    });
    engine.db.save_operation(&op).await.unwrap();
    assert!(engine.prune_cache(0).await.is_err());
    assert_eq!(
        std::fs::read(fixture.root.join("note.txt")).unwrap(),
        b"baseline"
    );
}

#[tokio::test]
async fn desktop_update_restart_drains_active_work_and_preserves_paused_service() {
    use mygdrive_sync::{
        dashboard::{self, Command, Event},
        service,
    };
    async fn ready(controller: &dashboard::Controller) -> bool {
        loop {
            while let Ok(event) = controller.events.try_recv() {
                match event {
                    Event::RestartReady { start_service } => return start_service,
                    Event::Completed(Err(error)) => panic!("restart failed: {error}"),
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    for paused in [false, true] {
        let fixture = Fixture::new(true);
        fixture.drive.hold_get.store(!paused, Ordering::SeqCst);
        let engine = Arc::new(fixture.engine().await);
        let task = tokio::spawn(service::run_owned(
            engine,
            fixture.state.clone(),
            service_config(&fixture),
            paused,
            CancellationToken::new(),
        ));
        if paused {
            service_wait(&fixture.state, |s| s.paused).await;
        } else {
            tokio::time::timeout(Duration::from_secs(5), fixture.drive.get_entered.notified())
                .await
                .unwrap();
        }
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_mygdrive-sync"));
        let controller =
            dashboard::spawn_observer(fixture.state.clone(), binary.clone(), false).unwrap();
        controller
            .commands
            .try_send(Command::RestartAfterUpdate)
            .unwrap();
        if !paused {
            service_wait(&fixture.state, |s| s.shutdown_requested).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(100), ready(&controller))
                    .await
                    .is_err()
            );
            assert!(
                !task.is_finished(),
                "an update must not cancel the active transfer"
            );
            fixture.drive.release_get.notify_one();
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(6), ready(&controller))
                .await
                .unwrap()
        );
        task.await.unwrap().unwrap();
        assert_eq!(service::last_status(&fixture.state).unwrap().paused, paused);
        if paused {
            dashboard::perform(&fixture.state, &binary, Command::Start)
                .await
                .unwrap();
            let resumed = service_wait(&fixture.state, |s| s.paused).await;
            assert_eq!(resumed.cycles_finished, 0);
            dashboard::perform(&fixture.state, &binary, Command::ShutdownAndWait)
                .await
                .unwrap();
        }
    }
    let fixture = Fixture::new(true);
    let controller =
        dashboard::spawn_observer(fixture.state.clone(), PathBuf::from("/nonexistent"), false)
            .unwrap();
    controller
        .commands
        .try_send(Command::RestartAfterUpdate)
        .unwrap();
    assert!(
        !tokio::time::timeout(Duration::from_secs(3), ready(&controller))
            .await
            .unwrap(),
        "a stopped service must stay stopped after update"
    );
}

#[tokio::test]
async fn multi_account_controls_are_targeted_and_update_restores_both_paused_profiles() {
    use mygdrive_sync::{
        account_manager,
        dashboard::{self, Command},
        oauth, service,
    };
    use std::os::unix::fs::PermissionsExt;
    let collection = tempfile::tempdir().unwrap();
    let base = collection.path().join("collection");
    let mut first = Fixture::new(true);
    let mut second = Fixture::new(true);
    first.state = base.join("profiles/first");
    second.state = base.join("profiles/second");
    let mut tasks = Vec::new();
    for fixture in [&first, &second] {
        let engine = Arc::new(fixture.engine().await);
        let token = oauth::token_path(&fixture.state, "test");
        std::fs::create_dir_all(token.parent().unwrap()).unwrap();
        std::fs::write(&token,serde_json::to_vec(&serde_json::json!({"client":{"client_id":"synthetic.apps.googleusercontent.com","client_secret":"synthetic"},"access_token":"synthetic","refresh_token":"synthetic","expires_at_ms":i64::MAX,"scope":oauth::DRIVE_SCOPE,"drive_user_id":fixture.state.to_string_lossy(),"identity":{"email":format!("{}@example.test",fixture.state.file_name().unwrap().to_string_lossy()),"display_name":"Synthetic"}})).unwrap()).unwrap();
        std::fs::set_permissions(token, std::fs::Permissions::from_mode(0o600)).unwrap();
        tasks.push(tokio::spawn(service::run_owned(
            engine,
            fixture.state.clone(),
            service_config(fixture),
            true,
            CancellationToken::new(),
        )));
        service_wait(&fixture.state, |s| s.paused).await;
    }
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_mygdrive-sync"));
    account_manager::control(&base, &first.state, &binary, Command::Resume)
        .await
        .unwrap();
    service_wait(&first.state, |s| !s.paused).await;
    assert!(
        dashboard::read_snapshot(&second.state)
            .await
            .status
            .unwrap()
            .paused
    );
    account_manager::control(&base, &first.state, &binary, Command::Pause)
        .await
        .unwrap();
    service_wait(&first.state, |s| s.paused).await;
    account_manager::drain_collection(&base, &binary, true)
        .await
        .unwrap();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert!(!dashboard::read_snapshot(&first.state).await.live);
    assert!(!dashboard::read_snapshot(&second.state).await.live);
    account_manager::restore_collection(&base, &binary)
        .await
        .unwrap();
    for fixture in [&first, &second] {
        let status = service_wait(&fixture.state, |s| s.paused).await;
        assert!(status.paused && !status.cycle_active);
    }
    account_manager::drain_collection(&base, &binary, false)
        .await
        .unwrap();
    assert!(!base.join("restart-accounts.json").exists());
}
