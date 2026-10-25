use async_trait::async_trait;
use bytes::Bytes;
use mygdrive_sync::{
    drive::{ByteStream, DriveClient},
    model::*,
    Error, Result, SyncEngine,
};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

struct TimedDrive {
    files: Mutex<HashMap<String, (RemoteFile, Vec<u8>)>>,
    delay_ms: AtomicUsize,
    get_calls: AtomicUsize,
    file_get_calls: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
    download_calls: AtomicUsize,
    trace: Mutex<Vec<String>>,
    hold: Mutex<Option<(String, Arc<GetGate>)>>,
    faults: Mutex<HashMap<String, GetFault>>,
}

struct GetGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

enum GetFault {
    Missing,
    Denied,
}

struct ActiveGet<'a>(&'a AtomicUsize);
impl Drop for ActiveGet<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl TimedDrive {
    fn new() -> Arc<Self> {
        let drive = Arc::new(Self {
            files: Mutex::new(HashMap::new()),
            delay_ms: AtomicUsize::new(0),
            get_calls: AtomicUsize::new(0),
            file_get_calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            download_calls: AtomicUsize::new(0),
            trace: Mutex::new(vec![]),
            hold: Mutex::new(None),
            faults: Mutex::new(HashMap::new()),
        });
        drive.files.lock().unwrap().insert(
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
        drive
    }

    fn put(&self, id: &str, name: &str, bytes: &[u8]) {
        let mut files = self.files.lock().unwrap();
        let version = files
            .get(id)
            .map_or(1, |(remote, _)| remote.version.parse::<u64>().unwrap() + 1);
        files.insert(
            id.into(),
            (
                RemoteFile {
                    id: id.into(),
                    name: name.into(),
                    mime_type: "text/plain".into(),
                    version: version.to_string(),
                    size: Some(bytes.len().to_string()),
                    sha256_checksum: Some(format!("{:x}", Sha256::digest(bytes))),
                    parents: vec!["root".into()],
                    capabilities: Capabilities {
                        can_download: Some(true),
                        can_edit: Some(true),
                        can_trash: Some(true),
                    },
                    ..Default::default()
                },
                bytes.to_vec(),
            ),
        );
    }

    fn reset_measurement(&self, delay_ms: usize) {
        assert_eq!(self.active.load(Ordering::SeqCst), 0);
        self.get_calls.store(0, Ordering::SeqCst);
        self.file_get_calls.store(0, Ordering::SeqCst);
        self.peak.store(0, Ordering::SeqCst);
        self.delay_ms.store(delay_ms, Ordering::SeqCst);
        self.trace.lock().unwrap().clear();
    }

    fn hold_next(&self, id: &str) -> Arc<GetGate> {
        let gate = Arc::new(GetGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *self.hold.lock().unwrap() = Some((id.into(), gate.clone()));
        gate
    }

    fn data(&self, id: &str) -> Vec<u8> {
        self.files.lock().unwrap()[id].1.clone()
    }
}

#[async_trait]
impl DriveClient for TimedDrive {
    async fn start_page_token(&self) -> Result<String> {
        Ok("start".into())
    }

    async fn list_files(&self, _: Option<&str>) -> Result<FilePage> {
        Ok(FilePage {
            files: self
                .files
                .lock()
                .unwrap()
                .values()
                .map(|(file, _)| file.clone())
                .collect(),
            next_page_token: None,
        })
    }

    async fn changes(&self, _: &str) -> Result<ChangePage> {
        Ok(ChangePage {
            changes: self
                .files
                .lock()
                .unwrap()
                .values()
                .map(|(file, _)| Change {
                    file_id: file.id.clone(),
                    removed: false,
                    file: Some(file.clone()),
                })
                .collect(),
            next_page_token: None,
            new_start_page_token: Some("next".into()),
        })
    }

    async fn get(&self, id: &str) -> Result<Option<RemoteFile>> {
        self.get_calls.fetch_add(1, Ordering::SeqCst);
        self.trace.lock().unwrap().push(format!("get:{id}"));
        let _active = if id != "root" {
            self.file_get_calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            Some(ActiveGet(&self.active))
        } else {
            None
        };
        // Capture a response before waiting so the stale-state tests cannot
        // accidentally refresh an old response when the wait is released.
        let response = self
            .files
            .lock()
            .unwrap()
            .get(id)
            .map(|(file, _)| file.clone());
        let gate = {
            let mut hold = self.hold.lock().unwrap();
            if hold.as_ref().is_some_and(|(target, _)| target == id) {
                hold.take().map(|(_, gate)| gate)
            } else {
                None
            }
        };
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        if id != "root" {
            let delay = self.delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay as u64)).await;
            }
        }
        match self.faults.lock().unwrap().remove(id) {
            Some(GetFault::Missing) => Ok(None),
            Some(GetFault::Denied) => Err(Error::Http {
                status: 403,
                reason: "permission denied".into(),
            }),
            None => Ok(response),
        }
    }

    async fn download(&self, id: &str) -> Result<ByteStream> {
        self.download_calls.fetch_add(1, Ordering::SeqCst);
        let bytes = self
            .files
            .lock()
            .unwrap()
            .get(id)
            .ok_or(Error::Changed)?
            .1
            .clone();
        Ok(Box::pin(futures_util::stream::once(async move {
            Ok(Bytes::from(bytes))
        })))
    }

    async fn reserve_id(&self) -> Result<String> {
        Err(Error::Unsupported("benchmark does not create files".into()))
    }

    async fn create(&self, _: &str, _: &str, _: &str, _: &Snapshot) -> Result<RemoteFile> {
        Err(Error::Unsupported("benchmark does not create files".into()))
    }

    fn conditional_mutations(&self) -> bool {
        true
    }

    async fn update(&self, expected: &RemoteFile, snapshot: &Snapshot) -> Result<RemoteFile> {
        let bytes = std::fs::read(&snapshot.path)?;
        let mut files = self.files.lock().unwrap();
        let (remote, current_bytes) = files.get_mut(&expected.id).ok_or(Error::Changed)?;
        if remote.version != expected.version {
            return Err(Error::Precondition);
        }
        remote.version = (remote.version.parse::<u64>().unwrap() + 1).to_string();
        remote.sha256_checksum = Some(format!("{:x}", Sha256::digest(&bytes)));
        remote.size = Some(bytes.len().to_string());
        *current_bytes = bytes;
        self.trace
            .lock()
            .unwrap()
            .push(format!("update:{}", expected.id));
        Ok(remote.clone())
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    drive: Arc<TimedDrive>,
    engine: Arc<SyncEngine>,
}

impl Fixture {
    async fn warm(count: usize) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sync");
        std::fs::create_dir(&root).unwrap();
        let drive = TimedDrive::new();
        for n in 0..count {
            drive.put(
                &format!("item-{n:03}"),
                &format!("file-{n:03}.txt"),
                b"baseline",
            );
        }
        let engine = Arc::new(
            SyncEngine::open(
                &root,
                &temp.path().join("state"),
                "test",
                "root",
                drive.clone(),
            )
            .await
            .unwrap(),
        );
        sqlx::query("UPDATE settings SET value='mirror' WHERE key='storage_mode'")
            .execute(&engine.db.pool)
            .await
            .unwrap();
        let report = engine.sync_once().await.unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(engine.db.materialized_files().await.unwrap().len(), count);
        Self {
            _temp: temp,
            root,
            drive,
            engine,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_latency_measurement_for_24_warmed_files() {
    let fixture = Fixture::warm(24).await;
    fixture.drive.reset_measurement(40);
    let started = Instant::now();
    let report = fixture.engine.sync_once().await.unwrap();
    let elapsed = started.elapsed();
    assert!(report.errors.is_empty(), "{report:?}");
    for n in 0..24 {
        assert_eq!(
            std::fs::read(fixture.root.join(format!("file-{n:03}.txt"))).unwrap(),
            b"baseline"
        );
    }
    println!(
        "metadata_latency files=24 simulated_rtt_ms=40 elapsed_ms={} total_gets={} file_gets={} peak_file_gets={}",
        elapsed.as_millis(),
        fixture.drive.get_calls.load(Ordering::SeqCst),
        fixture.drive.file_get_calls.load(Ordering::SeqCst),
        fixture.drive.peak.load(Ordering::SeqCst),
    );
    assert_eq!(fixture.drive.file_get_calls.load(Ordering::SeqCst), 24);
    assert!(fixture.drive.peak.load(Ordering::SeqCst) > 1);
    assert!(fixture.drive.peak.load(Ordering::SeqCst) <= 4);
    assert_eq!(fixture.drive.download_calls.load(Ordering::SeqCst), 24);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dirty_upload_commits_before_routine_clean_metadata_prefetch_begins() {
    let fixture = Fixture::warm(8).await;
    fixture.drive.reset_measurement(25);
    std::fs::write(fixture.root.join("file-007.txt"), b"edited first").unwrap();
    let report = fixture.engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(fixture.drive.data("item-007"), b"edited first");
    let trace = fixture.drive.trace.lock().unwrap().clone();
    let update = trace
        .iter()
        .position(|event| event == "update:item-007")
        .unwrap();
    let first_clean = trace
        .iter()
        .position(|event| event.starts_with("get:item-") && event != "get:item-007")
        .unwrap();
    assert!(
        update < first_clean,
        "clean checks overtook the pending save: {trace:?}"
    );
    assert!(fixture.drive.peak.load(Ordering::SeqCst) > 1);
    assert!(fixture.drive.peak.load(Ordering::SeqCst) <= 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_clean_metadata_request_does_not_block_ready_peer_content_updates() {
    let fixture = Fixture::warm(4).await;
    fixture.drive.reset_measurement(10);
    fixture
        .drive
        .put("item-001", "file-001.txt", b"updated on server");
    let gate = fixture.drive.hold_next("item-000");
    let engine = fixture.engine.clone();
    let task = tokio::spawn(async move { engine.sync_once().await });
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    let ready_path = fixture.root.join("file-001.txt");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if std::fs::read(&ready_path).unwrap() == b"updated on server" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a held metadata GET blocked a ready peer's server update");
    assert!(!task.is_finished());
    assert!(fixture.drive.active.load(Ordering::SeqCst) >= 1);
    gate.release.notify_one();
    let report = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(fixture.drive.active.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prefetched_metadata_is_refetched_when_a_local_save_changes_its_snapshot() {
    let fixture = Fixture::warm(4).await;
    fixture.drive.reset_measurement(10);
    let original = fixture
        .engine
        .db
        .by_remote("item-000")
        .await
        .unwrap()
        .unwrap();
    let gate = fixture.drive.hold_next("item-000");
    let engine = fixture.engine.clone();
    let task = tokio::spawn(async move { engine.sync_once().await });
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    std::fs::write(
        fixture.root.join("file-000.txt"),
        b"saved during metadata request",
    )
    .unwrap();
    fixture.engine.scanner.scan().await.unwrap();
    assert!(
        fixture
            .engine
            .db
            .file(&original.id)
            .await
            .unwrap()
            .generation
            > original.generation
    );
    gate.release.notify_one();
    let report = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        fixture.drive.data("item-000"),
        b"saved during metadata request"
    );
    assert_eq!(
        std::fs::read(fixture.root.join("file-000.txt")).unwrap(),
        b"saved during metadata request"
    );
    let trace = fixture.drive.trace.lock().unwrap().clone();
    assert_eq!(
        trace
            .iter()
            .filter(|event| event.as_str() == "get:item-000")
            .count(),
        2,
        "an invalidated prefetched response must be replaced with a fresh GET: {trace:?}"
    );
    assert_eq!(fixture.drive.download_calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        fixture.engine.db.file(&original.id).await.unwrap().status,
        "synced"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prefetched_missing_or_denied_metadata_preserves_local_bytes_and_can_retry() {
    for denied in [false, true] {
        let fixture = Fixture::warm(4).await;
        fixture.drive.reset_measurement(10);
        fixture.drive.faults.lock().unwrap().insert(
            "item-000".into(),
            if denied {
                GetFault::Denied
            } else {
                GetFault::Missing
            },
        );
        let result = fixture.engine.sync_once().await;
        if denied {
            assert!(matches!(result, Err(Error::Http { status: 403, .. })));
        } else {
            let report = result.unwrap();
            assert!(report.errors.is_empty(), "{report:?}");
            assert_eq!(
                fixture
                    .engine
                    .db
                    .by_remote("item-000")
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                "remote_unavailable"
            );
        }
        assert_eq!(fixture.drive.active.load(Ordering::SeqCst), 0);
        for n in 0..4 {
            assert_eq!(
                std::fs::read(fixture.root.join(format!("file-{n:03}.txt"))).unwrap(),
                b"baseline"
            );
            assert!(
                !fixture.drive.files.lock().unwrap()[&format!("item-{n:03}")]
                    .0
                    .trashed
            );
        }
        let report = fixture.engine.sync_once().await.unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(
            fixture
                .engine
                .db
                .by_remote("item-000")
                .await
                .unwrap()
                .unwrap()
                .status,
            "synced"
        );
        assert_eq!(fixture.drive.download_calls.load(Ordering::SeqCst), 4);
    }
}
