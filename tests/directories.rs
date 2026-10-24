use async_trait::async_trait;
use bytes::Bytes;
use mygdrive_sync::{
    directory_sync,
    drive::{ByteStream, DriveClient},
    model::*,
    Error, Result, SyncEngine,
};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

struct MemoryDrive {
    files: Mutex<HashMap<String, (RemoteFile, Vec<u8>)>>,
    next: AtomicUsize,
    lose_create_response: AtomicBool,
}
impl MemoryDrive {
    fn new() -> Arc<Self> {
        let drive = Arc::new(Self {
            files: Mutex::new(HashMap::new()),
            next: AtomicUsize::new(1),
            lose_create_response: AtomicBool::new(false),
        });
        drive.folder("root", "root", "");
        drive
    }
    fn folder(&self, id: &str, name: &str, parent: &str) {
        self.files.lock().unwrap().insert(
            id.into(),
            (
                RemoteFile {
                    id: id.into(),
                    name: name.into(),
                    mime_type: "application/vnd.google-apps.folder".into(),
                    version: "1".into(),
                    parents: if parent.is_empty() {
                        vec![]
                    } else {
                        vec![parent.into()]
                    },
                    ..Default::default()
                },
                vec![],
            ),
        );
    }
    fn by_name(&self, name: &str) -> RemoteFile {
        self.files
            .lock()
            .unwrap()
            .values()
            .find(|(f, _)| f.name == name && !f.trashed)
            .unwrap()
            .0
            .clone()
    }
    fn mutate(&self, expected: &RemoteFile, f: impl FnOnce(&mut RemoteFile)) -> Result<RemoteFile> {
        let mut files = self.files.lock().unwrap();
        let current = &mut files.get_mut(&expected.id).unwrap().0;
        if current.version != expected.version {
            return Err(Error::Precondition);
        }
        f(current);
        current.version = (current.version.parse::<usize>().unwrap() + 1).to_string();
        Ok(current.clone())
    }
}
#[async_trait]
impl DriveClient for MemoryDrive {
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
                .map(|(f, _)| f.clone())
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
        Ok(self.files.lock().unwrap().get(id).map(|(f, _)| f.clone()))
    }
    async fn list_children(&self, id: &str) -> Result<Vec<RemoteFile>> {
        Ok(self
            .files
            .lock()
            .unwrap()
            .values()
            .filter(|(f, _)| !f.trashed && f.parents.iter().any(|p| p == id))
            .map(|(f, _)| f.clone())
            .collect())
    }
    async fn reserve_id(&self) -> Result<String> {
        Ok(format!("new{}", self.next.fetch_add(1, Ordering::SeqCst)))
    }
    async fn download(&self, id: &str) -> Result<ByteStream> {
        let bytes = self.files.lock().unwrap()[id].1.clone();
        Ok(Box::pin(futures_util::stream::once(async move {
            Ok(Bytes::from(bytes))
        })))
    }
    async fn create_folder(&self, id: &str, name: &str, parent: &str) -> Result<RemoteFile> {
        if self.get(id).await?.is_none() {
            self.folder(id, name, parent);
        }
        if self.lose_create_response.swap(false, Ordering::SeqCst) {
            return Err(Error::Http {
                status: 503,
                reason: "lost committed response".into(),
            });
        }
        Ok(self.get(id).await?.unwrap())
    }
    async fn create(
        &self,
        id: &str,
        name: &str,
        parent: &str,
        snapshot: &Snapshot,
    ) -> Result<RemoteFile> {
        let bytes = std::fs::read(&snapshot.path)?;
        let remote = RemoteFile {
            id: id.into(),
            name: name.into(),
            parents: vec![parent.into()],
            version: "1".into(),
            mime_type: "text/plain".into(),
            sha256_checksum: Some(format!("{:x}", Sha256::digest(&bytes))),
            size: Some(bytes.len().to_string()),
            ..Default::default()
        };
        self.files
            .lock()
            .unwrap()
            .insert(id.into(), (remote.clone(), bytes));
        Ok(remote)
    }
    fn conditional_mutations(&self) -> bool {
        true
    }
    async fn update(&self, expected: &RemoteFile, snapshot: &Snapshot) -> Result<RemoteFile> {
        let bytes = std::fs::read(&snapshot.path)?;
        let updated = self.mutate(expected, |f| {
            f.sha256_checksum = Some(format!("{:x}", Sha256::digest(&bytes)));
            f.size = Some(bytes.len().to_string());
        })?;
        self.files.lock().unwrap().get_mut(&expected.id).unwrap().1 = bytes;
        Ok(updated)
    }
    async fn rename(&self, expected: &RemoteFile, name: &str, parent: &str) -> Result<RemoteFile> {
        self.mutate(expected, |f| {
            f.name = name.into();
            f.parents = vec![parent.into()];
        })
    }
    async fn trash(&self, expected: &RemoteFile) -> Result<RemoteFile> {
        self.mutate(expected, |f| f.trashed = true)
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    engine: SyncEngine,
    drive: Arc<MemoryDrive>,
}
impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        std::fs::create_dir(&root).unwrap();
        let drive = MemoryDrive::new();
        let engine = SyncEngine::open(
            &root,
            &temp.path().join("state"),
            "test",
            "root",
            drive.clone(),
        )
        .await
        .unwrap();
        Self {
            _temp: temp,
            root,
            engine,
            drive,
        }
    }
    async fn sync(&self) {
        let report = self.engine.sync_once().await.unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
    }
}

#[tokio::test]
async fn empty_directory_create_rename_trash_roundtrip() {
    let f = Fixture::new().await;
    std::fs::create_dir(f.root.join("empty")).unwrap();
    f.sync().await;
    let id = f.drive.by_name("empty").id;
    std::fs::rename(f.root.join("empty"), f.root.join("renamed")).unwrap();
    f.sync().await;
    assert_eq!(f.drive.by_name("renamed").id, id);
    std::fs::remove_dir(f.root.join("renamed")).unwrap();
    f.sync().await;
    assert!(f.drive.get(&id).await.unwrap().unwrap().trashed);
}

#[tokio::test]
async fn nested_folder_rename_keeps_dirty_file_and_cloud_identity() {
    let f = Fixture::new().await;
    std::fs::create_dir_all(f.root.join("a/nested")).unwrap();
    std::fs::write(f.root.join("a/nested/file.txt"), b"original").unwrap();
    f.sync().await;
    let folder = f.drive.by_name("a");
    let file = f.drive.by_name("file.txt");
    std::fs::rename(f.root.join("a"), f.root.join("b")).unwrap();
    std::fs::write(
        f.root.join("b/nested/file.txt"),
        b"edited alongside folder rename",
    )
    .unwrap();
    f.sync().await;
    assert_eq!(f.drive.by_name("b").id, folder.id);
    assert_eq!(f.drive.by_name("file.txt").id, file.id);
    assert_eq!(
        f.drive.files.lock().unwrap()[&file.id].1,
        b"edited alongside folder rename"
    );
    std::fs::remove_dir_all(f.root.join("b")).unwrap();
    f.sync().await;
    assert!(f.drive.get(&file.id).await.unwrap().unwrap().trashed);
    assert!(f.drive.get(&folder.id).await.unwrap().unwrap().trashed);
}

#[tokio::test]
async fn existing_materialized_cloud_folder_is_adopted_without_duplicate() {
    let f = Fixture::new().await;
    f.drive.folder("existing-test-id", "MyGDrive-Test", "root");
    std::fs::create_dir(f.root.join("MyGDrive-Test")).unwrap();
    f.sync().await;
    let dirs = directory_sync::directories(&f.engine.db).await.unwrap();
    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0].remote_id.as_deref(), Some("existing-test-id"));
    assert_eq!(f.drive.files.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn unknown_new_cloud_child_prevents_parent_trash() {
    let f = Fixture::new().await;
    std::fs::create_dir(f.root.join("empty")).unwrap();
    f.sync().await;
    let folder = f.drive.by_name("empty");
    std::fs::remove_dir(f.root.join("empty")).unwrap();
    f.engine.scanner.scan().await.unwrap();
    // A server-side child arriving AFTER the local inventory, unseen by SQLite.
    f.drive
        .folder("new-remote-child", "do-not-trash", &folder.id);
    let errors =
        directory_sync::reconcile_after(&f.engine.db, &f.engine.scanner, f.drive.as_ref(), "root")
            .await
            .unwrap();
    assert_eq!(errors.len(), 1);
    assert!(!f.drive.get(&folder.id).await.unwrap().unwrap().trashed);
    assert!(
        !f.drive
            .get("new-remote-child")
            .await
            .unwrap()
            .unwrap()
            .trashed
    );
}

#[tokio::test]
async fn lost_create_response_reuses_reserved_id_and_latest_offline_rename() {
    let f = Fixture::new().await;
    std::fs::create_dir(f.root.join("first")).unwrap();
    f.drive.lose_create_response.store(true, Ordering::SeqCst);
    let report = f.engine.sync_once().await.unwrap();
    assert!(!report.errors.is_empty());
    let id = f.drive.by_name("first").id;
    std::fs::rename(f.root.join("first"), f.root.join("second")).unwrap();
    std::fs::rename(f.root.join("second"), f.root.join("latest")).unwrap();
    f.sync().await;
    assert_eq!(f.drive.by_name("latest").id, id);
    assert_eq!(f.drive.files.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn remote_folder_rename_moves_materialized_tree_without_copying_or_conflicts() {
    let f = Fixture::new().await;
    std::fs::create_dir_all(f.root.join("before/nested")).unwrap();
    std::fs::write(f.root.join("before/nested/file.txt"), b"bytes").unwrap();
    f.sync().await;
    let folder = f.drive.by_name("before");
    let file = f.drive.by_name("file.txt");
    f.drive.rename(&folder, "after", "root").await.unwrap();
    f.sync().await;
    assert_eq!(
        std::fs::read(f.root.join("after/nested/file.txt")).unwrap(),
        b"bytes"
    );
    assert!(!f.root.join("before").exists());
    let local = f.engine.db.by_remote(&file.id).await.unwrap().unwrap();
    assert_eq!(local.relative_path, "after/nested/file.txt");
    assert!(!local.review_required);
    assert_eq!(f.drive.files.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn remote_empty_folder_trash_removes_only_empty_local_directory() {
    let f = Fixture::new().await;
    std::fs::create_dir(f.root.join("empty")).unwrap();
    f.sync().await;
    let remote = f.drive.by_name("empty");
    f.drive.trash(&remote).await.unwrap();
    f.sync().await;
    assert!(!f.root.join("empty").exists());
}

#[tokio::test]
async fn folder_move_recovery_after_filesystem_commit_preserves_dirty_file() {
    let f = Fixture::new().await;
    std::fs::create_dir_all(f.root.join("before/nested")).unwrap();
    std::fs::write(f.root.join("before/nested/file.txt"), b"original").unwrap();
    f.sync().await;
    let folder = f.drive.by_name("before");
    let file = f.drive.by_name("file.txt");
    std::fs::write(
        f.root.join("before/nested/file.txt"),
        b"unsynced editor bytes",
    )
    .unwrap();
    let remote = f.drive.rename(&folder, "after", "root").await.unwrap();
    let mut directory = directory_sync::directories(&f.engine.db)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.relative_path == "before")
        .unwrap();
    directory.move_journal = Some(directory_sync::DirectoryMove {
        source: "before".into(),
        target: "after".into(),
        remote,
    });
    sqlx::query("UPDATE materialized_directories SET data=? WHERE id=?")
        .bind(serde_json::to_string(&directory).unwrap())
        .bind(&directory.id)
        .execute(&f.engine.db.pool)
        .await
        .unwrap();
    std::fs::rename(f.root.join("before"), f.root.join("after")).unwrap();
    // Simulate a restart between rename(2) and the SQLite baseline transaction.
    f.engine.scanner.scan().await.unwrap();
    let local = f.engine.db.by_remote(&file.id).await.unwrap().unwrap();
    assert_eq!(local.relative_path, "after/nested/file.txt");
    assert_eq!(
        local.baseline.as_ref().unwrap().relative_path,
        local.relative_path
    );
    assert_ne!(
        local.fingerprint.as_ref().unwrap().digest,
        local.baseline.as_ref().unwrap().digest
    );
    f.sync().await;
    assert_eq!(
        f.drive.files.lock().unwrap()[&file.id].1,
        b"unsynced editor bytes"
    );
}

#[tokio::test]
async fn simultaneous_local_and_cloud_folder_renames_fail_closed_preserving_bytes() {
    let f = Fixture::new().await;
    std::fs::create_dir(f.root.join("before")).unwrap();
    std::fs::write(f.root.join("before/file.txt"), b"original").unwrap();
    f.sync().await;
    let folder = f.drive.by_name("before");
    std::fs::rename(f.root.join("before"), f.root.join("local-name")).unwrap();
    f.drive.rename(&folder, "cloud-name", "root").await.unwrap();
    f.engine.scanner.scan().await.unwrap();
    let errors =
        directory_sync::reconcile_before(&f.engine.db, &f.engine.scanner, f.drive.as_ref(), "root")
            .await
            .unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        std::fs::read(f.root.join("local-name/file.txt")).unwrap(),
        b"original"
    );
    assert_eq!(f.drive.by_name("cloud-name").id, folder.id);
    assert!(directory_sync::directories(&f.engine.db)
        .await
        .unwrap()
        .iter()
        .any(|d| d.review_required));
}

#[tokio::test]
async fn cloud_only_folder_and_scope_root_absence_are_never_local_deletions() {
    let f = Fixture::new().await;
    f.drive.folder("cloud-only", "not-downloaded", "root");
    f.sync().await;
    f.sync().await;
    assert!(directory_sync::directories(&f.engine.db)
        .await
        .unwrap()
        .is_empty());
    assert!(!f.drive.get("cloud-only").await.unwrap().unwrap().trashed);
    assert!(!f.drive.get("root").await.unwrap().unwrap().trashed);
}

#[tokio::test]
async fn unicode_directory_move_rebases_all_nested_files_and_new_empty_children() {
    let f = Fixture::new().await;
    std::fs::create_dir_all(f.root.join("arsip-é/内訳/deep")).unwrap();
    std::fs::write(f.root.join("arsip-é/内訳/deep/😀.txt"), b"unicode paths").unwrap();
    f.sync().await;
    let folder = f.drive.by_name("arsip-é");
    let file = f.drive.by_name("😀.txt");
    f.drive.rename(&folder, "資料", "root").await.unwrap();
    f.sync().await;
    assert_eq!(
        std::fs::read(f.root.join("資料/内訳/deep/😀.txt")).unwrap(),
        b"unicode paths"
    );
    let local = f.engine.db.by_remote(&file.id).await.unwrap().unwrap();
    assert_eq!(local.relative_path, "資料/内訳/deep/😀.txt");
    assert_eq!(
        local.baseline.as_ref().unwrap().relative_path,
        local.relative_path
    );
    std::fs::rename(f.root.join("資料"), f.root.join("資料最新")).unwrap();
    std::fs::create_dir(f.root.join("資料最新/内訳/deep/空")).unwrap();
    f.sync().await;
    let empty = f.drive.by_name("空");
    assert_eq!(empty.parents, vec![f.drive.by_name("deep").id]);
    assert_eq!(f.drive.by_name("資料最新").id, folder.id);
}

#[tokio::test]
async fn lost_create_response_then_offline_delete_trashes_same_reserved_folder() {
    let f = Fixture::new().await;
    std::fs::create_dir(f.root.join("temporary")).unwrap();
    f.drive.lose_create_response.store(true, Ordering::SeqCst);
    let report = f.engine.sync_once().await.unwrap();
    assert!(!report.errors.is_empty());
    let id = f.drive.by_name("temporary").id;
    std::fs::remove_dir(f.root.join("temporary")).unwrap();
    f.sync().await;
    assert!(f.drive.get(&id).await.unwrap().unwrap().trashed);
    assert_eq!(f.drive.files.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn parent_and_child_independent_renames_and_edit_preserve_cloud_ids() {
    let f = Fixture::new().await;
    std::fs::create_dir_all(f.root.join("a/inner")).unwrap();
    std::fs::write(f.root.join("a/inner/x.txt"), b"original").unwrap();
    f.sync().await;
    let parent = f.drive.by_name("a");
    let child = f.drive.by_name("inner");
    let file = f.drive.by_name("x.txt");
    std::fs::rename(f.root.join("a"), f.root.join("b")).unwrap();
    std::fs::rename(f.root.join("b/inner"), f.root.join("b/child")).unwrap();
    std::fs::rename(f.root.join("b/child/x.txt"), f.root.join("b/child/y.txt")).unwrap();
    std::fs::write(f.root.join("b/child/y.txt"), b"renamed and edited").unwrap();
    f.sync().await;
    let pending = f.engine.db.by_remote(&file.id).await.unwrap().unwrap();
    assert_ne!(
        pending.status, "synced",
        "content upload must retain rename intent"
    );
    for _ in 0..2 {
        f.sync().await;
    }
    assert_eq!(f.drive.by_name("b").id, parent.id);
    assert_eq!(f.drive.by_name("child").id, child.id);
    assert_eq!(f.drive.by_name("y.txt").id, file.id);
    assert_eq!(
        f.drive.files.lock().unwrap()[&file.id].1,
        b"renamed and edited"
    );
    assert_eq!(f.drive.files.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn remote_folder_move_into_cloud_only_parent_materializes_and_adopts_ancestors() {
    let f = Fixture::new().await;
    std::fs::create_dir_all(f.root.join("before/nested")).unwrap();
    std::fs::write(f.root.join("before/nested/file.txt"), b"keep nested bytes").unwrap();
    f.sync().await;
    let folder = f.drive.by_name("before");
    let file = f.drive.by_name("file.txt");
    f.drive.folder("cloud-parent", "online-parent", "root");
    f.drive
        .folder("cloud-inner", "online-inner", "cloud-parent");
    assert!(!f.root.join("online-parent").exists());
    f.drive
        .rename(&folder, "after", "cloud-inner")
        .await
        .unwrap();
    f.sync().await;
    f.sync().await;
    assert_eq!(
        std::fs::read(
            f.root
                .join("online-parent/online-inner/after/nested/file.txt")
        )
        .unwrap(),
        b"keep nested bytes"
    );
    assert!(!f.root.join("before").exists());
    let local = f.engine.db.by_remote(&file.id).await.unwrap().unwrap();
    assert_eq!(
        local.relative_path,
        "online-parent/online-inner/after/nested/file.txt"
    );
    let dirs = directory_sync::directories(&f.engine.db).await.unwrap();
    for (path, id) in [
        ("online-parent", "cloud-parent"),
        ("online-parent/online-inner", "cloud-inner"),
    ] {
        let directory = dirs.iter().find(|d| d.relative_path == path).unwrap();
        assert_eq!(directory.remote_id.as_deref(), Some(id));
        assert!(!directory.review_required);
    }
    assert_eq!(f.drive.by_name("after").id, folder.id);
    assert_eq!(f.drive.files.lock().unwrap().len(), 6);
}

#[tokio::test]
async fn lost_folder_creation_then_server_rename_is_not_acknowledged_at_wrong_path() {
    let f = Fixture::new().await;
    std::fs::create_dir(f.root.join("first")).unwrap();
    f.drive.lose_create_response.store(true, Ordering::SeqCst);
    assert!(!f.engine.sync_once().await.unwrap().errors.is_empty());
    let remote = f.drive.by_name("first");
    f.drive
        .rename(&remote, "server-name", "root")
        .await
        .unwrap();
    let report = f.engine.sync_once().await.unwrap();
    assert!(!report.errors.is_empty());
    assert!(f.root.join("first").is_dir());
    assert_eq!(f.drive.by_name("server-name").id, remote.id);
    let dirs = directory_sync::directories(&f.engine.db).await.unwrap();
    let directory = dirs
        .iter()
        .find(|d| d.remote_id.as_deref() == Some(remote.id.as_str()))
        .unwrap();
    assert!(directory.review_required);
    assert!(directory.baseline.is_none());
    assert_eq!(f.drive.files.lock().unwrap().len(), 2);
}
