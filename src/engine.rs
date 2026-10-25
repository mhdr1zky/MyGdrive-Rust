use crate::{
    cache::CacheStore,
    db::Database,
    drive::{file_name, DriveClient},
    local::{self, Scanner},
    model::*,
    reconcile::{reconcile, Decision},
    Error, Result,
};
use fs2::FileExt;
use futures_util::{stream::FuturesUnordered, StreamExt};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    fs::{File, OpenOptions},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, RwLock, Semaphore};

const ROUTINE_METADATA_PARALLELISM: usize = 4;

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncReport {
    pub completed: usize,
    pub conflicts: usize,
    pub deferred: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncProgress {
    pub stage: String,
    pub inventory_pages: u64,
    pub inventory_items: u64,
    #[serde(default)]
    pub active_operations: u64,
    #[serde(default)]
    pub last_activity_ms: i64,
}

impl SyncProgress {
    /// Brief grace period makes short transfers visible between desktop polls.
    pub fn has_file_activity(&self, now_ms: i64) -> bool {
        self.active_operations > 0
            || (self.last_activity_ms > 0
                && (0..2500).contains(&now_ms.saturating_sub(self.last_activity_ms)))
    }
}

pub struct SyncEngine {
    pub scanner: Scanner,
    pub cache: CacheStore,
    pub db: Database,
    drive: Arc<dyn DriveClient>,
    folder: String,
    cycle: Mutex<()>,
    files: crate::keyed_lock::KeyedLocks,
    hydration_slots: Semaphore,
    hydration_lifecycle: RwLock<()>,
    folder_index: Mutex<HashMap<String, String>>,
    _process_lock: File,
    progress: std::sync::RwLock<SyncProgress>,
    activity: Arc<crate::activity::Activity>,
}

impl SyncEngine {
    fn routine_metadata_candidate(file: &LocalFile) -> bool {
        let (Some(fingerprint), Some(baseline), Some(remote_id)) =
            (&file.fingerprint, &file.baseline, &file.remote_id)
        else {
            return false;
        };
        file.status == "synced"
            && file.materialized
            && !file.review_required
            && file.relative_path == baseline.relative_path
            && fingerprint.digest == baseline.digest
            && remote_id == &baseline.remote.id
            && !baseline.remote.trashed
            && !baseline.remote.unavailable
    }

    fn metadata_snapshot_matches(snapshot: &LocalFile, current: &LocalFile) -> bool {
        Self::routine_metadata_candidate(current)
            && snapshot.id == current.id
            && snapshot.generation == current.generation
            && snapshot.relative_path == current.relative_path
            && snapshot.remote_id == current.remote_id
            && snapshot.fingerprint == current.fingerprint
            && snapshot.status == current.status
            && snapshot.materialized == current.materialized
            && snapshot.review_required == current.review_required
            && snapshot.available_offline == current.available_offline
            && match (&snapshot.baseline, &current.baseline) {
                (Some(before), Some(after)) => {
                    before.digest == after.digest
                        && before.relative_path == after.relative_path
                        && before.remote_path == after.remote_path
                        && before.remote == after.remote
                }
                _ => false,
            }
    }

    async fn routine_metadata(
        &self,
        snapshot: LocalFile,
    ) -> (LocalFile, Result<Option<RemoteFile>>) {
        let result = self
            .drive
            .get(
                snapshot
                    .remote_id
                    .as_deref()
                    .expect("routine file has remote ID"),
            )
            .await;
        (snapshot, result)
    }

    pub fn progress(&self) -> SyncProgress {
        let mut progress = self.progress.read().unwrap().clone();
        (progress.active_operations, progress.last_activity_ms) = self.activity.snapshot();
        progress
    }
    fn stage(&self, stage: &str) {
        self.progress.write().unwrap().stage = stage.into();
    }

    pub async fn prune_cache(&self, max_bytes: u64) -> Result<crate::cache::EvictionReport> {
        let _guard = self.cycle.lock().await;
        let _hydrations = self.hydration_lifecycle.write().await;
        self.recover_moves().await?;
        self.scanner.scan().await?;
        self.cache.prune(max_bytes).await
    }

    pub async fn set_available_offline(&self, id: &str, pinned: bool) -> Result<()> {
        let _guard = self.cycle.lock().await;
        let _file = self.files.lock(id).await;
        self.recover_moves().await?;
        self.scanner.scan().await?;
        let _guard = self.scanner.gate.lock().await;
        let mut file = self.db.file(id).await?;
        // Pin records intent, including for a metadata-only file. Availability is
        // reported separately; the content still waits for a successful sync.
        file.available_offline = pinned;
        self.db.save_file(&file).await?;
        let _policy = self.cache.policy_gate.lock().await;
        let graph = crate::offline::Graph::load(&self.db, false).await?;
        let target = graph
            .target_for_file(id)
            .ok_or_else(|| Error::Invalid("file not indexed".into()))?;
        graph
            .set(&self.db, &uuid::Uuid::new_v4().to_string(), &target, pinned)
            .await?;
        Ok(())
    }
    pub async fn offline_page(&self, parent: &str, offset: usize) -> Result<crate::offline::Page> {
        Self::offline_page_from(&self.db, parent, offset).await
    }
    pub(crate) async fn offline_page_from(
        db: &Database,
        parent: &str,
        offset: usize,
    ) -> Result<crate::offline::Page> {
        crate::offline::Graph::load(db, true)
            .await?
            .page(parent, offset)
    }
    pub async fn offline_select(
        &self,
        request_id: &str,
        target: &str,
        pinned: bool,
    ) -> Result<bool> {
        let _policy = self.cache.policy_gate.lock().await;
        crate::offline::Graph::load(&self.db, false)
            .await?
            .set(&self.db, request_id, target, pinned)
            .await
    }

    /// Check ancestry with indexed single-row lookups, rather than loading the
    /// account graph for every open. This uses the same first-parent/depth
    /// rules as project_paths; current file metadata is checked again from API.
    async fn hydration_in_scope(&self, remote: &RemoteFile) -> Result<bool> {
        let root = self.root_id().await?;
        let mut parent = remote.parents.first().cloned();
        let mut visited = HashSet::from([remote.id.clone()]);
        for _ in 0..128 {
            let Some(id) = parent else { return Ok(false) };
            if id == root {
                return Ok(true);
            }
            if !visited.insert(id.clone()) {
                return Ok(false);
            }
            let data: Option<String> =
                sqlx::query_scalar("SELECT data FROM remote_observations WHERE id=?")
                    .bind(id)
                    .fetch_optional(&self.db.pool)
                    .await?;
            let Some(data) = data else { return Ok(false) };
            let ancestor: RemoteFile = serde_json::from_str(&data)?;
            if ancestor.trashed || ancestor.unavailable || !ancestor.folder() {
                return Ok(false);
            }
            parent = ancestor.parents.first().cloned();
        }
        Ok(false)
    }

    async fn verify_local_root(&self) -> Result<()> {
        let meta = std::fs::symlink_metadata(&self.scanner.root)?;
        let identity = format!("{}:{}", meta.dev(), meta.ino());
        if !meta.is_dir() || self.db.setting("root_identity").await?.as_deref() != Some(&identity) {
            return Err(Error::Changed);
        }
        Ok(())
    }

    async fn hydration_local(&self, file: &LocalFile) -> Result<bool> {
        if let Some(id) = &file.remote_id {
            let data: Option<String> =
                sqlx::query_scalar("SELECT data FROM remote_observations WHERE id=?")
                    .bind(id)
                    .fetch_optional(&self.db.pool)
                    .await?;
            let remote: RemoteFile = serde_json::from_str(&data.ok_or(Error::Changed)?)?;
            if remote.trashed || remote.unavailable || !self.hydration_in_scope(&remote).await? {
                return Err(Error::Changed);
            }
        }
        let path = local::safe_path(&self.scanner.root, &file.relative_path)?;
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() && meta.nlink() == 1 => Ok(true),
            Ok(_) => Err(Error::Changed),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Explicit opens bypass the background cycle. Same-file requests coalesce
    /// with background work; unrelated files get two bounded foreground slots.
    /// Installation still uses the journal, generation and scanner gate.
    pub async fn hydrate(&self, id: &str) -> Result<()> {
        let _lifecycle = self.hydration_lifecycle.read().await;
        {
            let _guard = self.scanner.gate.lock().await;
            crate::dehydration::recover(&self.db, &self.scanner.root).await?;
        }
        self.verify_local_root().await?;
        // Available bytes do not wait behind an upload of this same file.
        if self.hydration_local(&self.db.file(id).await?).await? {
            return Ok(());
        }
        let _file = self.files.lock(id).await;
        let file = self.db.file(id).await?;
        if self.hydration_local(&file).await? {
            return Ok(());
        }
        let _slot = self
            .hydration_slots
            .acquire()
            .await
            .expect("hydration semaphore open");
        let pending = self.db.file_operations(id).await?;
        if !pending.is_empty() {
            // A stopped/failed download is recoverable with its existing journal.
            // Never execute an upload, rename or deletion as an implicit open.
            if pending.len() != 1 || pending[0].kind != "download" {
                return Err(Error::Changed);
            }
            let mut op = pending.into_iter().next().unwrap();
            return self.execute_hydration(&mut op).await;
        }
        if file.materialized || file.fingerprint.is_some() || file.review_required {
            return Err(Error::Changed);
        }
        let remote_id = file.remote_id.as_ref().ok_or(Error::Changed)?;
        let remote = self.drive.get(remote_id).await?.ok_or(Error::Changed)?;
        if remote.trashed
            || remote.unavailable
            || remote.workspace()
            || remote.folder()
            || remote.capabilities.can_download == Some(false)
        {
            return Err(Error::Unsupported("Drive file cannot be downloaded".into()));
        }
        if !self.hydration_in_scope(&remote).await? {
            return Err(Error::Changed);
        }
        let mut op = {
            // Metadata intents and directory rebases also take this gate. Do not
            // publish a stale generation if a rename/delete arrived during GET.
            let _guard = self.scanner.gate.lock().await;
            let mut current = self.db.file(id).await?;
            if current.generation != file.generation || current.relative_path != file.relative_path
            {
                return Err(Error::Changed);
            }
            if self.hydration_local(&current).await? {
                return Ok(());
            }
            current.generation += 1;
            self.db.save_file(&current).await?;
            self.db
                .operation(&Operation::new(&current, "download", Some(remote)))
                .await?
        };
        self.execute_hydration(&mut op).await
    }

    /// Record a cloud-only rename/delete without waiting for any network or cycle lock.
    pub async fn virtual_metadata(
        &self,
        request_id: &str,
        id: &str,
        source: &str,
        destination: Option<&str>,
    ) -> Result<bool> {
        if uuid::Uuid::parse_str(request_id).is_err() {
            return Err(Error::Invalid("invalid metadata request ID".into()));
        }
        let _guard = self.scanner.gate.lock().await;
        local::safe_path(&self.scanner.root, source)?;
        if let Some(path) = destination {
            local::safe_path(&self.scanner.root, path)?;
        }
        let payload = serde_json::to_string(&("virtual_metadata", id, source, destination))?;
        let mut tx = self.db.pool.begin().await?;
        if let Some(old) = sqlx::query_scalar::<_, String>(
            "SELECT payload FROM offline_commands WHERE request_id=?",
        )
        .bind(request_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            if old != payload {
                return Err(Error::Invalid(
                    "request ID reused with different metadata intent".into(),
                ));
            }
            return Ok(true);
        }
        let data: String = sqlx::query_scalar("SELECT data FROM files WHERE id=?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        let mut file: LocalFile = serde_json::from_str(&data)?;
        if !matches!(
            file.status.as_str(),
            "online_only" | "remote_only" | "modified_local"
        ) || file.relative_path != source
            || file.materialized
            || file.fingerprint.is_some()
            || file.review_required
            || local::safe_path(&self.scanner.root, source)?.try_exists()?
        {
            return Err(Error::Changed);
        }
        let remote_id = file.remote_id.as_ref().ok_or(Error::Changed)?;
        if file.baseline.is_none() {
            let data: String =
                sqlx::query_scalar("SELECT data FROM remote_observations WHERE id=?")
                    .bind(remote_id)
                    .fetch_one(&mut *tx)
                    .await?;
            let remote: RemoteFile = serde_json::from_str(&data)?;
            if remote.folder() || remote.workspace() || remote.trashed || remote.unavailable {
                return Err(Error::Unsupported(
                    "cloud metadata mutation unavailable".into(),
                ));
            }
            file.baseline = Some(Baseline {
                digest: crate::model::Digest {
                    sha256: remote.sha256_checksum.clone().unwrap_or_default(),
                    md5: remote.md5_checksum.clone().unwrap_or_default(),
                    size: remote
                        .size
                        .as_deref()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                },
                relative_path: source.into(),
                remote_path: None,
                remote,
            });
        }
        if let Some(path) = destination {
            if sqlx::query_scalar::<_,i64>("SELECT count(*) FROM files WHERE relative_path=? AND id!=? AND sync_status!='tombstone'").bind(path).bind(id).fetch_one(&mut *tx).await?>0 {return Err(Error::Invalid("rename destination exists".into()));}
            file.relative_path = path.into();
            file.status = "modified_local".into();
        } else {
            file.materialized = true;
            file.status = "deleted_local".into();
        }
        file.generation += 1;
        Database::write_file(&mut tx, &self.db.account, &file).await?;
        sqlx::query("INSERT INTO offline_commands(request_id,payload) VALUES(?,?)")
            .bind(request_id)
            .bind(payload)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(false)
    }

    pub async fn approve_recovery(&self, id: &str) -> Result<()> {
        let _guard = self.cycle.lock().await;
        let _file = self.files.lock(id).await;
        self.recover_moves().await?;
        self.scanner.scan().await?;
        let _guard = self.scanner.gate.lock().await;
        let mut file = self.db.file(id).await?;
        if !file.review_required && file.status != "recovery_review" {
            return Err(Error::Invalid(
                "file is not awaiting recovery review".into(),
            ));
        }
        file.fingerprint = Some(local::fingerprint(&local::safe_path(
            &self.scanner.root,
            &file.relative_path,
        )?)?);
        file.review_required = false;
        file.generation += 1;
        file.status = crate::db::status(&file);
        self.db.save_file(&file).await
    }
    pub async fn open(
        root: &Path,
        state: &Path,
        account: &str,
        folder: &str,
        drive: Arc<dyn DriveClient>,
    ) -> Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(Error::Invalid("sync root must be a directory".into()));
        }
        local::private_dir(state)?;
        let state = state.canonicalize()?;
        if state.starts_with(&root) || root.starts_with(&state) {
            return Err(Error::Invalid(
                "state and sync root must be disjoint".into(),
            ));
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(state.join("engine.lock"))?;
        lock.try_lock_exclusive()
            .map_err(|_| Error::Invalid("another engine is using this state directory".into()))?;
        // Protect DB/WAL creation independently of the caller's umask.
        let db_path = state.join("state.sqlite3");
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&db_path)?;
        let db = Database::open(&db_path, account).await?;
        if db.setting("scope_migration_journal").await?.is_some() {
            return Err(Error::Invalid("Migrasi scope belum selesai. Jalankan expand-my-drive --apply untuk recovery sebelum memulai service.".into()));
        }
        db.bind(
            "root",
            root.to_str()
                .ok_or_else(|| Error::Invalid("root UTF-8".into()))?,
        )
        .await?;
        db.bind("folder", folder).await?;
        let scanner = Scanner {
            root,
            db: db.clone(),
            gate: Arc::new(Mutex::new(())),
        };
        crate::dehydration::recover(&db, &scanner.root).await?;
        let cache = CacheStore::new(state, db.clone())?;
        let activity = Arc::new(crate::activity::Activity::default());
        let drive = Arc::new(crate::activity::TrackedDrive {
            inner: drive,
            activity: activity.clone(),
        });
        Ok(Self {
            scanner,
            cache,
            db,
            drive,
            folder: folder.into(),
            cycle: Mutex::new(()),
            files: crate::keyed_lock::KeyedLocks::default(),
            hydration_slots: Semaphore::new(2),
            hydration_lifecycle: RwLock::new(()),
            folder_index: Mutex::new(HashMap::new()),
            _process_lock: lock,
            progress: std::sync::RwLock::new(SyncProgress::default()),
            activity,
        })
    }

    async fn root_id(&self) -> Result<String> {
        self.db
            .setting("resolved_folder")
            .await?
            .ok_or_else(|| Error::Invalid("remote folder not resolved".into()))
    }

    async fn fetch_changes(&self) -> Result<()> {
        let folder = self
            .drive
            .get(&self.folder)
            .await?
            .ok_or_else(|| Error::Invalid("selected Drive folder is inaccessible".into()))?;
        if !folder.folder() {
            return Err(Error::Invalid(
                "selected remote item is not a folder".into(),
            ));
        }
        self.db.bind("resolved_folder", &folder.id).await?;
        if self.db.cursor("changes").await?.is_none() {
            let token = if let Some(t) = self.db.cursor("bootstrap_start").await? {
                t
            } else {
                let t = self.drive.start_page_token().await?;
                self.db.remote_page(&[], "bootstrap_start", &t).await?;
                t
            };
            let mut page = self.db.cursor("inventory").await?;
            if page.as_deref() != Some("complete") {
                loop {
                    let result = self
                        .drive
                        .list_files(page.as_deref())
                        .await
                        .map_err(cursor_error)?;
                    let next = result.next_page_token.clone();
                    self.db
                        .remote_page(
                            &result.files,
                            "inventory",
                            next.as_deref().unwrap_or("complete"),
                        )
                        .await?;
                    {
                        let mut progress = self.progress.write().unwrap();
                        progress.inventory_pages += 1;
                        progress.inventory_items += result.files.len() as u64;
                    }
                    page = next;
                    if page.is_none() {
                        break;
                    }
                }
            }
            self.db.remote_page(&[], "changes", &token).await?;
        }
        self.stage("change_feed");
        let mut token = self.db.cursor("changes").await?.unwrap();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(token.clone()) {
                return Err(Error::Invalid("repeated change page token".into()));
            }
            let page = self.drive.changes(&token).await.map_err(cursor_error)?;
            let remotes: Vec<_> = page
                .changes
                .into_iter()
                .map(|change| {
                    let mut file = change.file.unwrap_or_else(|| RemoteFile {
                        id: change.file_id,
                        ..Default::default()
                    });
                    if change.removed {
                        file.unavailable = true;
                    }
                    file
                })
                .collect();
            let next = page
                .next_page_token
                .clone()
                .or(page.new_start_page_token)
                .ok_or_else(|| Error::Invalid("change page has no continuation token".into()))?;
            self.db.remote_page(&remotes, "changes", &next).await?;
            token = next;
            if page.next_page_token.is_none() {
                break;
            }
        }
        Ok(())
    }

    pub async fn sync_once(&self) -> Result<SyncReport> {
        let _cycle = self.cycle.lock().await;
        *self.progress.write().unwrap() = SyncProgress {
            stage: "local_scan".into(),
            ..Default::default()
        };
        self.recover_moves().await?;
        self.recover_displaced().await?;
        self.scanner.scan().await?;
        self.stage("inventory");
        match self.fetch_changes().await {
            Err(Error::CursorRejected) => {
                self.db.reset_inventory().await?;
                self.fetch_changes().await?;
            }
            result => result?,
        }
        self.stage("reconciling");
        self.folder_index.lock().await.clear();
        let mut report = SyncReport::default();
        let root_id = self.root_id().await?;
        report.errors.extend(
            crate::directory_sync::reconcile_before(
                &self.db,
                &self.scanner,
                self.drive.as_ref(),
                &root_id,
            )
            .await?,
        );
        let (initial_requested, pinned_directories) = {
            let _policy = self.cache.policy_gate.lock().await;
            let graph = crate::offline::Graph::load(&self.db, false).await?;
            graph.promote(&self.db).await?;
            (graph.requested_file_ids(), graph.requested_directory_ids())
        };
        let mut active = HashSet::new();
        let online_default = self.db.setting("storage_mode").await?.as_deref() != Some("mirror");
        for snapshot in self.db.operations().await? {
            let _file = self.files.lock(&snapshot.file_id).await;
            // An explicit open may have completed this journal while the cycle
            // was busy with a different file. Never replay the stale snapshot.
            let mut op = self.db.operation_by_id(&snapshot.id).await?;
            if matches!(op.stage.as_str(), "done" | "cancelled") {
                continue;
            }
            let file = self.db.file(&op.file_id).await?;
            if online_default
                && op.kind == "download"
                && op.stage == "planned"
                && !file.materialized
                && file.fingerprint.is_none()
                && !initial_requested.contains(&file.id)
            {
                op.stage = "cancelled".into();
                self.db.save_operation(&op).await?;
                continue;
            }
            active.insert(op.file_id.clone());
            if op.next_retry_ms > crate::now_ms() {
                report.deferred += 1;
                continue;
            }
            self.execute_reported(&mut op, &mut report).await?;
        }
        let remotes = self.db.remotes().await?;
        let root_id = self.root_id().await?;
        let paths = project_paths(&remotes, &root_id);
        if !pinned_directories.is_empty() {
            let _guard = self.scanner.gate.lock().await;
            self.verify_local_root().await?;
            crate::directory_sync::materialize_pinned(
                &self.db,
                &self.scanner.root,
                &remotes,
                &paths,
                &pinned_directories,
            )
            .await?;
        }
        let remote_by_id: HashMap<_, _> = remotes.iter().map(|r| (r.id.as_str(), r)).collect();
        {
            let mut index = self.folder_index.lock().await;
            index.clear();
            index.insert(String::new(), root_id.clone());
            for folder in remotes
                .iter()
                .filter(|r| r.folder() && !r.trashed && !r.unavailable)
            {
                if let Some(path) = paths.get(&folder.id) {
                    index.insert(path.clone(), folder.id.clone());
                }
            }
        }
        // Register remote-only records before planning downloads. Local name collisions get a new path.
        // Registration only needs identities and occupied paths. Decoding every
        // LocalFile here retained a second full account snapshot throughout sync.
        let registered: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT relative_path,remote_id FROM files")
                .fetch_all(&self.db.pool)
                .await?;
        let known: HashSet<_> = registered
            .iter()
            .filter_map(|(_, remote_id)| remote_id.as_deref())
            .collect();
        let mut occupied: HashSet<String> =
            registered.iter().map(|(path, _)| path.clone()).collect();
        let mut additions = Vec::new();
        for remote in &remotes {
            if remote.folder() || remote.trashed || remote.unavailable {
                continue;
            }
            let Some(path) = paths.get(&remote.id) else {
                continue;
            };
            if known.contains(remote.id.as_str()) {
                continue;
            }
            let mut relative = path.clone();
            let mut n = 0;
            while occupied.contains(&relative)
                || local::safe_path(&self.scanner.root, &relative)?.try_exists()?
            {
                n += 1;
                relative = conflict_name(path, &format!("Drive-{}-{n}", short_hash(&remote.id)));
            }
            occupied.insert(relative.clone());
            additions.push(LocalFile {
                id: uuid::Uuid::new_v4().to_string(),
                relative_path: relative,
                remote_id: Some(remote.id.clone()),
                generation: 0,
                fingerprint: None,
                baseline: None,
                status: if online_default {
                    "online_only"
                } else if remote.workspace() {
                    "unsupported"
                } else {
                    "remote_only"
                }
                .into(),
                materialized: false,
                review_required: false,
                available_offline: false,
            });
        }
        self.db.register_files(&additions).await?;
        let has_additions = !additions.is_empty();
        drop(known);
        drop(occupied);
        drop(registered);
        drop(additions);
        let requested = if !has_additions {
            initial_requested
        } else {
            crate::offline::Graph::load(&self.db, false)
                .await?
                .requested_file_ids()
        };
        let mut candidates = self
            .db
            .reconciliation_files(&requested, !online_default)
            .await?;
        // Stable priority preserves relative order within a class, but never
        // puts a new save behind routine checks of already synchronized files.
        candidates.sort_by_key(|file| {
            if matches!(
                file.status.as_str(),
                "new_local" | "modified_local" | "deleted_local"
            ) {
                0
            } else if !file.materialized && requested.contains(&file.id) {
                1
            } else {
                2
            }
        });
        let mut candidates = candidates.into_iter().peekable();
        let mut metadata = FuturesUnordered::new();
        loop {
            // Dirty saves and requested downloads retain their stable priority.
            // Only consecutive clean checks share the bounded network window;
            // each result still reconciles serially under its usual file lock.
            while metadata.len() < ROUTINE_METADATA_PARALLELISM
                && candidates.peek().is_some_and(|file| {
                    !active.contains(&file.id) && Self::routine_metadata_candidate(file)
                })
            {
                metadata.push(self.routine_metadata(candidates.next().unwrap()));
            }
            let (snapshot, prefetched) = if !metadata.is_empty() {
                let (snapshot, result) = metadata.next().await.unwrap();
                (snapshot, Some(result))
            } else {
                let Some(snapshot) = candidates.next() else {
                    break;
                };
                (snapshot, None)
            };
            // Most streamed-account rows are untouched online-only metadata.
            // They require no mutation, lock or extra SELECT. If an explicit
            // open materializes one concurrently, it owns its journal; a later
            // scan/cycle will reconcile subsequent edits from the fresh row.
            if active.contains(&snapshot.id)
                || (online_default
                    && snapshot.status == "online_only"
                    && !snapshot.materialized
                    && snapshot.fingerprint.is_none()
                    && !requested.contains(&snapshot.id)
                    && !snapshot
                        .baseline
                        .as_ref()
                        .is_some_and(|base| snapshot.relative_path != base.relative_path))
            {
                continue;
            }
            let _file = self.files.lock(&snapshot.id).await;
            let mut file = self.db.file(&snapshot.id).await?;
            // Metadata-only records are not local deletions. Never create empty placeholders.
            // Existing materialized copies and local edits retain the original reconciliation path.
            if online_default
                && !file.materialized
                && file.fingerprint.is_none()
                && !requested.contains(&file.id)
                && !file
                    .baseline
                    .as_ref()
                    .is_some_and(|base| file.relative_path != base.relative_path)
            {
                if file.status != "online_only" {
                    self.set_status(&file.id, "online_only").await?;
                }
                continue;
            }
            if file.status == "online_only"
                && !file.materialized
                && file.fingerprint.is_none()
                && !file
                    .baseline
                    .as_ref()
                    .is_some_and(|base| file.relative_path != base.relative_path)
            {
                let _guard = self.scanner.gate.lock().await;
                file = self.db.file(&file.id).await?;
                if file.status == "online_only" && !file.materialized && file.fingerprint.is_none()
                {
                    file.generation += 1;
                    file.status = "remote_only".into();
                    self.db.save_file(&file).await?;
                }
            }
            if file.review_required || file.status == "recovery_review" {
                report.deferred += 1;
                continue;
            }
            let remote = match &file.remote_id {
                Some(id) => {
                    // Prefetch owns no file lock. A save, pin change or metadata
                    // intent invalidates its snapshot and requires a fresh GET.
                    let current = match prefetched {
                        Some(result) if Self::metadata_snapshot_matches(&snapshot, &file) => {
                            result?
                        }
                        _ => self.drive.get(id).await?,
                    };
                    match current {
                        Some(remote) => Some(remote),
                        None => {
                            let mut inaccessible = remote_by_id
                                .get(id.as_str())
                                .copied()
                                .cloned()
                                .unwrap_or_default();
                            inaccessible.id = id.clone();
                            inaccessible.unavailable = true;
                            Some(inaccessible)
                        }
                    }
                }
                None => None,
            };
            // Content/version-only changes cannot change ancestry. Reuse the
            // account projection instead of cloning/rebuilding 80k rows per file.
            let projection = if let Some(current) = &remote {
                if remote_by_id.get(current.id.as_str()).is_some_and(|known| {
                    known.metadata_eq(current)
                        && known.trashed == current.trashed
                        && known.unavailable == current.unavailable
                }) {
                    paths.get(&current.id).cloned()
                } else {
                    let mut latest = remotes.clone();
                    latest.retain(|r| r.id != current.id);
                    latest.push(current.clone());
                    project_paths(&latest, &root_id).get(&current.id).cloned()
                }
            } else {
                None
            };
            if let Some(r) = &remote {
                if !r.unavailable && !r.trashed && projection.is_none() {
                    self.set_status(&file.id, "out_of_scope").await?;
                    report.deferred += 1;
                    continue;
                }
            }
            if let (Some(base), Some(remote), Some(projected)) =
                (&file.baseline, &remote, &projection)
            {
                if !remote.trashed && !remote.unavailable && file.fingerprint.is_some() {
                    let remote_moved = !remote.metadata_eq(&base.remote)
                        || base
                            .remote_path
                            .as_ref()
                            .is_some_and(|old| old != projected);
                    if remote_moved {
                        if file.relative_path != base.relative_path
                            && &file.relative_path != projected
                        {
                            self.set_status(&file.id, "metadata_conflict_review")
                                .await?;
                            report.deferred += 1;
                            continue;
                        }
                        let mut planned =
                            Operation::new(&file, "remote_move", Some(remote.clone()));
                        planned.projected_path = Some(projected.clone());
                        planned.key.push_str(&format!(":{}", short_hash(projected)));
                        let mut op = self.db.operation(&planned).await?;
                        if op.stage != "done" {
                            self.execute_reported(&mut op, &mut report).await?;
                        }
                        if op.stage != "done" {
                            continue;
                        }
                        file = self.db.file(&file.id).await?;
                    } else if base.remote_path.is_none() {
                        file.baseline.as_mut().unwrap().remote_path = Some(projected.clone());
                        let _guard = self.scanner.gate.lock().await;
                        let mut current = self.db.file(&file.id).await?;
                        if let Some(base) = &mut current.baseline {
                            base.remote_path = Some(projected.clone());
                        }
                        self.db.save_file(&current).await?;
                        file = current;
                    }
                }
            }
            let mut r = remote;
            let mut decision = reconcile(&file, r.as_ref());
            if decision == Decision::NeedRemoteHash {
                let remote = r.as_mut().unwrap();
                let snapshot = self
                    .cache
                    .download_to_cache(self.drive.as_ref(), remote)
                    .await?;
                remote.sha256_checksum = Some(snapshot.digest.sha256);
                decision = reconcile(&file, Some(remote));
            }
            // Downloads may coexist with local metadata intent; apply only server-originated moves automatically.
            let kind = match decision {
                Decision::Nothing => continue,
                Decision::Unavailable => {
                    self.set_status(&file.id, "remote_unavailable").await?;
                    report.deferred += 1;
                    continue;
                }
                Decision::Unsupported => {
                    self.set_status(&file.id, "unsupported").await?;
                    report.deferred += 1;
                    continue;
                }
                Decision::Adopt => {
                    if let (Some(fp), Some(remote)) = (&file.fingerprint, r) {
                        self.db
                            .acknowledge(
                                &file.id,
                                file.generation,
                                fp.digest.clone(),
                                remote,
                                file.relative_path.clone(),
                                projection.clone(),
                            )
                            .await?;
                    } else if file.fingerprint.is_none() {
                        self.set_status(&file.id, "tombstone").await?;
                    }
                    continue;
                }
                Decision::Create => "create",
                Decision::Upload if self.drive.conditional_mutations() => "upload",
                Decision::Upload | Decision::Conflict => "conflict",
                Decision::Download => "download",
                Decision::Trash if self.drive.conditional_mutations() => "trash",
                Decision::Trash => {
                    self.set_status(&file.id, "delete_pending_validation")
                        .await?;
                    report.deferred += 1;
                    continue;
                }
                Decision::RemoteTrash => "remote_trash",
                Decision::DeleteEditConflict => {
                    // Never resurrect a server deletion or access loss automatically.
                    self.set_status(&file.id, "delete_edit_review").await?;
                    report.deferred += 1;
                    continue;
                }
                Decision::Rename => {
                    if self.drive.conditional_mutations() {
                        "rename"
                    } else {
                        self.set_status(&file.id, "metadata_pending_validation")
                            .await?;
                        report.deferred += 1;
                        continue;
                    }
                }
                Decision::MetadataConflict => {
                    self.set_status(&file.id, "metadata_conflict_review")
                        .await?;
                    report.deferred += 1;
                    continue;
                }
                Decision::NeedRemoteHash => unreachable!(),
            };
            let mut planned = Operation::new(&file, kind, r);
            planned.projected_path = projection;
            let mut op = self.db.operation(&planned).await?;
            if matches!(op.stage.as_str(), "done" | "cancelled") {
                continue;
            }
            self.execute_reported(&mut op, &mut report).await?;
        }
        report.errors.extend(
            crate::directory_sync::reconcile_after(
                &self.db,
                &self.scanner,
                self.drive.as_ref(),
                &root_id,
            )
            .await?,
        );
        sqlx::query("INSERT INTO settings(key,value) VALUES('offline_verified_ms',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .bind(crate::now_ms().to_string()).execute(&self.db.pool).await?;
        if let Err(error) = self.release_unpinned().await {
            tracing::warn!(%error, "unpinned local cleanup deferred");
            report.errors.push("unpinned local cleanup deferred".into());
        }
        Ok(report)
    }

    /// Run after reconciliation so queued edits are uploaded before their clean
    /// local copy is released. Pin changes stay fast and survive a restart.
    async fn release_unpinned(&self) -> Result<()> {
        if self.db.setting("storage_mode").await?.as_deref() == Some("mirror") {
            return Ok(());
        }
        let ids: Vec<(String, String)> =
            sqlx::query_as("SELECT file_id,request_id FROM dehydration_requests ORDER BY rowid")
                .fetch_all(&self.db.pool)
                .await?;
        if ids.is_empty() {
            return Ok(());
        }
        let (mut revision, mut requested) = {
            let _policy = self.cache.policy_gate.lock().await;
            (
                self.selection_revision().await?,
                crate::offline::Graph::load(&self.db, false)
                    .await?
                    .requested_file_ids(),
            )
        };
        let mut released = HashSet::new();
        let mut completed = Vec::new();
        for (id, request_id) in ids {
            let before = self.db.file(&id).await?;
            if before.review_required
                || !matches!(before.status.as_str(), "synced" | "online_only")
                || !self.db.file_operations(&id).await?.is_empty()
            {
                continue;
            }
            let Some(base) = &before.baseline else {
                // A metadata-only file has never occupied the backing root.
                if !before.materialized && before.fingerprint.is_none() {
                    completed.push((id, request_id));
                }
                continue;
            };
            if before.relative_path != base.relative_path {
                continue;
            }
            let Some(remote_id) = &before.remote_id else {
                continue;
            };
            let Some(remote) = self.drive.get(remote_id).await? else {
                continue;
            };
            if remote.trashed
                || remote.unavailable
                || remote.workspace()
                || remote.folder()
                || remote.capabilities.can_download == Some(false)
                || remote.version != base.remote.version
                || remote.version.is_empty()
                || !remote.metadata_eq(&base.remote)
                || remote.matches(&base.digest) != Some(true)
                || !self.hydration_in_scope(&remote).await?
            {
                continue;
            }
            // Hydration takes lifecycle then file; preserve that lock order.
            let _lifecycle = self.hydration_lifecycle.write().await;
            let _file = self.files.lock(&id).await;
            let _guard = self.scanner.gate.lock().await;
            crate::dehydration::recover(&self.db, &self.scanner.root).await?;
            let _policy = self.cache.policy_gate.lock().await;
            let current_revision = self.selection_revision().await?;
            if revision != current_revision {
                requested = crate::offline::Graph::load(&self.db, false)
                    .await?
                    .requested_file_ids();
                revision = current_revision;
            }
            if requested.contains(&id) {
                completed.push((id, request_id));
                continue;
            }
            let current = self.db.file(&id).await?;
            if current.generation != before.generation
                || current.relative_path != before.relative_path
                || current.review_required
                || !self.db.file_operations(&id).await?.is_empty()
            {
                continue;
            }
            match crate::dehydration::evict(&self.db, &self.scanner.root, &current).await {
                Ok(true) => {
                    released.insert(remote_id.clone());
                    completed.push((id, request_id));
                }
                Ok(false) => (),
                Err(error) => {
                    crate::dehydration::recover(&self.db, &self.scanner.root).await?;
                    return Err(error);
                }
            }
        }
        // CacheStore takes its cache gate before pin policy. Never invert that
        // order, and recheck pins/active operations before discarding objects.
        self.cache.remove_for_remote_ids(&released).await?;
        for (id, request_id) in completed {
            sqlx::query("DELETE FROM dehydration_requests WHERE file_id=? AND request_id=? AND journal IS NULL")
                .bind(id)
                .bind(request_id)
                .execute(&self.db.pool)
                .await?;
        }
        Ok(())
    }

    async fn selection_revision(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT coalesce(max(rowid),0) FROM offline_commands")
                .fetch_one(&self.db.pool)
                .await?,
        )
    }

    async fn set_status(&self, id: &str, value: &str) -> Result<()> {
        let _guard = self.scanner.gate.lock().await;
        let mut current = self.db.file(id).await?;
        if current.status == value {
            return Ok(());
        }
        current.status = value.into();
        self.db.save_file(&current).await
    }

    async fn record_operation_failure(&self, op: &mut Operation, error: &Error) -> Result<bool> {
        let cancelled = matches!(error, Error::Changed | Error::Precondition)
            && (op.install.is_none() || op.stage == "cancelled");
        if cancelled {
            // A changed source must be replanned from the latest generation.
            op.stage = "cancelled".into();
            op.last_error = Some("source or remote changed; replanning".into());
            op.key = format!("cancelled:{}:{}", op.key, op.id);
        } else {
            op.attempts = op.attempts.saturating_add(1);
            op.next_retry_ms =
                crate::now_ms().saturating_add(retry_delay_ms(error, op.attempts, &op.id));
            op.last_error = Some(error.to_string());
        }
        self.db.save_operation(op).await?;
        Ok(cancelled)
    }

    async fn execute_hydration(&self, op: &mut Operation) -> Result<()> {
        let result = self.execute(op).await;
        if let Err(error) = &result {
            self.record_operation_failure(op, error).await?;
        }
        result
    }

    async fn execute_reported(&self, op: &mut Operation, report: &mut SyncReport) -> Result<()> {
        match self.execute(op).await {
            Ok(()) => {
                report.completed += 1;
                if op.kind == "conflict" {
                    report.conflicts += 1;
                }
            }
            Err(error) => {
                if self.record_operation_failure(op, &error).await? {
                    report.deferred += 1;
                } else {
                    report.errors.push(format!("{}: {}", op.file_id, error));
                }
            }
        }
        Ok(())
    }

    async fn execute(&self, op: &mut Operation) -> Result<()> {
        let file = self.db.file(&op.file_id).await?;
        if op.kind == "remote_move" {
            return self.execute_move(op).await;
        }
        if op.stage == "planned" && op.install.is_none() && file.generation != op.generation {
            return Err(Error::Changed);
        }
        if op.kind == "remote_trash" {
            return self.remote_trash(op, &file).await;
        }
        if op.kind == "rename" {
            let target = op
                .local_path
                .clone()
                .unwrap_or_else(|| file.relative_path.clone());
            let digest = file.baseline.as_ref().ok_or(Error::Changed)?.digest.clone();
            if op.stage != "remote_committed" {
                let parent = self.ensure_parent(&target).await?;
                let remote = self
                    .drive
                    .rename(
                        op.remote.as_ref().unwrap(),
                        file_name(Path::new(&target))?,
                        &parent,
                    )
                    .await?;
                if remote.matches(&digest) != Some(true) {
                    return Err(Error::Precondition);
                }
                op.remote = Some(remote);
                op.stage = "remote_committed".into();
                self.db.save_operation(op).await?;
            }
            self.scanner.scan().await?;
            self.db
                .acknowledge(
                    &file.id,
                    op.generation,
                    digest,
                    op.remote.clone().unwrap(),
                    target,
                    None,
                )
                .await?;
            op.stage = "done".into();
            self.db.save_operation(op).await?;
            return Ok(());
        }
        if matches!(op.kind.as_str(), "create" | "upload" | "conflict") {
            if op.snapshot.is_none() {
                let source = local::safe_path(&self.scanner.root, &file.relative_path)?;
                let snapshot = self.cache.snapshot(&source).await?;
                if file.fingerprint.as_ref().map(|f| &f.digest) != Some(&snapshot.digest) {
                    return Err(Error::Changed);
                }
                op.snapshot = Some(snapshot);
                op.stage = "snapshot_ready".into();
                self.db.save_operation(op).await?;
            }
            if op.kind == "conflict" {
                self.ensure_conflict_copy(op, &file).await?;
            }
            if op.stage != "remote_committed" && op.stage != "installed" && op.install.is_none() {
                let snapshot = op.snapshot.as_ref().unwrap();
                let uploaded = if op.kind == "upload" {
                    self.drive
                        .update(op.remote.as_ref().unwrap(), snapshot)
                        .await?
                } else {
                    if op.target_remote_id.is_none() {
                        op.target_remote_id = Some(self.drive.reserve_id().await?);
                        self.db.save_operation(op).await?;
                    }
                    let relative = op.conflict_path.as_deref().unwrap_or(&file.relative_path);
                    let parent = self.ensure_parent(relative).await?;
                    self.drive
                        .create(
                            op.target_remote_id.as_ref().unwrap(),
                            file_name(Path::new(relative))?,
                            &parent,
                            snapshot,
                        )
                        .await?
                };
                if uploaded.matches(&snapshot.digest) != Some(true) {
                    return Err(Error::Precondition);
                }
                // Persist the server result BEFORE acknowledging local content.
                if op.kind != "conflict" {
                    op.remote = Some(uploaded.clone());
                }
                op.stage = "remote_committed".into();
                self.db.save_operation(op).await?;
                if op.kind == "conflict" {
                    let copy = self
                        .db
                        .files()
                        .await?
                        .into_iter()
                        .find(|f| Some(&f.relative_path) == op.conflict_path.as_ref())
                        .ok_or(Error::Changed)?;
                    self.db
                        .acknowledge(
                            &copy.id,
                            copy.generation,
                            snapshot.digest.clone(),
                            uploaded,
                            copy.relative_path,
                            None,
                        )
                        .await?;
                }
            }
            if op.kind == "conflict"
                && matches!(op.stage.as_str(), "remote_committed" | "installed")
            {
                // Also runs on replay after a crash between server commit and local copy ACK.
                let uploaded = self
                    .drive
                    .get(op.target_remote_id.as_ref().unwrap())
                    .await?
                    .ok_or(Error::Changed)?;
                let copy_id = op
                    .conflict_file_id
                    .as_ref()
                    .ok_or_else(|| Error::Invalid("missing conflict copy identity".into()))?;
                let copy = self.db.file(copy_id).await?;
                self.db
                    .acknowledge(
                        copy_id,
                        copy.generation,
                        op.snapshot.as_ref().unwrap().digest.clone(),
                        uploaded,
                        op.conflict_path.clone().unwrap(),
                        None,
                    )
                    .await?;
            }
            if op.kind != "conflict" {
                // Re-scan actual bytes before ACK; watcher events can lag behind an editor.
                self.scanner.scan().await?;
                self.db
                    .acknowledge(
                        &file.id,
                        op.generation,
                        op.snapshot.as_ref().unwrap().digest.clone(),
                        op.remote.clone().unwrap(),
                        file.baseline
                            .as_ref()
                            .map(|base| base.relative_path.clone())
                            .unwrap_or_else(|| file.relative_path.clone()),
                        op.projected_path.clone(),
                    )
                    .await?;
                op.stage = "done".into();
                self.db.save_operation(op).await?;
                return Ok(());
            }
        }
        if op.kind == "trash" {
            self.drive.trash(op.remote.as_ref().unwrap()).await?;
            // A local recreation/edit can arrive while the server request is in
            // flight. Preserve it for review rather than hiding it as a tombstone.
            self.scanner.scan().await?;
            let _guard = self.scanner.gate.lock().await;
            let mut current = self.db.file(&file.id).await?;
            if current.generation != op.generation || current.fingerprint.is_some() {
                current.status = "delete_edit_review".into();
                current.review_required = true;
            } else {
                current.status = "tombstone".into();
            }
            self.db.save_file(&current).await?;
            op.stage = "done".into();
            self.db.save_operation(op).await?;
            return Ok(());
        }
        if op.downloaded.is_none() {
            let current = self
                .drive
                .get(&op.remote.as_ref().unwrap().id)
                .await?
                .ok_or(Error::Changed)?;
            if current.trashed || current.unavailable || !self.hydration_in_scope(&current).await? {
                return Err(Error::Changed);
            }
            // A newer server version is safe to preserve at the original path, after saving local bytes.
            op.remote = Some(current.clone());
            op.downloaded = Some(
                self.cache
                    .download_to_cache(self.drive.as_ref(), &current)
                    .await?,
            );
            self.db.save_operation(op).await?;
        }
        self.install(op).await?;
        if op.kind == "conflict" {
            self.set_status(&file.id, "conflict_review").await?;
        }
        op.stage = "done".into();
        self.db.save_operation(op).await?;
        Ok(())
    }

    async fn ensure_conflict_copy(&self, op: &mut Operation, original: &LocalFile) -> Result<()> {
        if let Some(copy_id) = &op.conflict_file_id {
            let copy = self.db.file(copy_id).await?;
            let path = local::safe_path(&self.scanner.root, &copy.relative_path)?;
            if !path.try_exists()? {
                return Err(Error::Unsupported(
                    "conflict copy was removed; immutable snapshot retained for review".into(),
                ));
            }
            let _guard = self.scanner.gate.lock().await;
            self.db
                .record_local(&copy.relative_path, Some(local::fingerprint(&path)?))
                .await?;
            return Ok(());
        }
        let snapshot = op.snapshot.as_ref().unwrap().clone();
        if op.conflict_path.is_none() {
            let simple = conflict_name(&original.relative_path, "Conflict");
            let occupied = self
                .db
                .files()
                .await?
                .iter()
                .any(|f| f.relative_path == simple);
            op.conflict_path = Some(
                if occupied || local::safe_path(&self.scanner.root, &simple)?.try_exists()? {
                    conflict_name(&original.relative_path, &format!("Conflict - {}", op.id))
                } else {
                    simple
                },
            );
            self.db.save_operation(op).await?;
        }
        let mut relative = op.conflict_path.clone().unwrap();
        let mut destination = local::safe_path(&self.scanner.root, &relative)?;
        let _guard = self.scanner.gate.lock().await;
        if destination.try_exists()? && local::fingerprint(&destination)?.digest != snapshot.digest
        {
            relative = conflict_name(
                &original.relative_path,
                &format!("Conflict - {}", uuid::Uuid::new_v4()),
            );
            destination = local::safe_path(&self.scanner.root, &relative)?;
            op.conflict_path = Some(relative.clone());
            self.db.save_operation(op).await?;
        }
        if !destination.try_exists()? {
            local::durable_copy(&snapshot.path, &destination)?;
        }
        let fp = local::fingerprint(&destination)?;
        if fp.digest != snapshot.digest && op.conflict_file_id.is_none() {
            return Err(Error::Changed);
        }
        self.db.record_local(&relative, Some(fp)).await?;
        if op.conflict_file_id.is_none() {
            op.conflict_file_id = self
                .db
                .files()
                .await?
                .into_iter()
                .find(|f| f.relative_path == relative)
                .map(|f| f.id);
            self.db.save_operation(op).await?;
        }
        self.db.conflict(op, &relative).await?;
        Ok(())
    }

    async fn unique_remote_path(&self, proposed: &str, id: &str) -> Result<String> {
        let files = self.db.files().await?;
        let mut path = proposed.to_string();
        let suffix = short_hash(id);
        let mut n = 0;
        while files.iter().any(|f| f.relative_path == path)
            || local::safe_path(&self.scanner.root, &path)?.try_exists()?
        {
            n += 1;
            path = conflict_name(proposed, &format!("Drive-{suffix}-{n}"));
        }
        Ok(path)
    }

    async fn ensure_parent(&self, relative: &str) -> Result<String> {
        let parent = Path::new(relative).parent().unwrap_or(Path::new(""));
        let mut remote_parent = self.root_id().await?;
        let mut local_parent = PathBuf::new();
        let mut index = self.folder_index.lock().await;
        // Replayed operations may execute before the main registration phase.
        if index.is_empty() {
            let remotes = self.db.remotes().await?;
            let paths = project_paths(&remotes, &remote_parent);
            for folder in remotes
                .iter()
                .filter(|f| f.folder() && !f.trashed && !f.unavailable)
            {
                if let Some(path) = paths.get(&folder.id) {
                    index.insert(path.clone(), folder.id.clone());
                }
            }
            index.insert(String::new(), remote_parent.clone());
        }
        for component in parent.components() {
            local_parent.push(component);
            let local_str = local_parent
                .to_str()
                .ok_or_else(|| Error::Invalid("parent UTF-8".into()))?;
            if let Some(folder) = index.get(local_str) {
                remote_parent = folder.clone();
                continue;
            }
            let name = component
                .as_os_str()
                .to_str()
                .ok_or_else(|| Error::Invalid("folder name".into()))?;
            let setting = format!("folder_id:{local_str}");
            let reserved = self.db.setting(&setting).await?;
            let id = if let Some(id) = reserved {
                match self.drive.get(&id).await? {
                    Some(existing)
                        if !existing.folder()
                            || existing.trashed
                            || existing.name != name
                            || existing.parents.first() != Some(&remote_parent) =>
                    {
                        let replacement = self.drive.reserve_id().await?;
                        sqlx::query("UPDATE settings SET value=? WHERE key=?")
                            .bind(&replacement)
                            .bind(&setting)
                            .execute(&self.db.pool)
                            .await?;
                        replacement
                    }
                    _ => id,
                }
            } else {
                let id = self.drive.reserve_id().await?;
                self.db.bind(&setting, &id).await?;
                id
            };
            let folder = self.drive.create_folder(&id, name, &remote_parent).await?;
            self.db
                .remote_page(&[folder], "created_folder", &id)
                .await?;
            index.insert(local_str.to_string(), id.clone());
            remote_parent = id;
        }
        Ok(remote_parent)
    }

    async fn execute_move(&self, op: &mut Operation) -> Result<()> {
        let _guard = self.scanner.gate.lock().await;
        let mut updated = self.db.file(&op.file_id).await?;
        let projected = op
            .projected_path
            .as_ref()
            .ok_or_else(|| Error::Invalid("missing remote move destination".into()))?;
        if op.move_journal.is_none() {
            let source = local::safe_path(&self.scanner.root, &updated.relative_path)?;
            let fp = local::fingerprint(&source)?;
            let target = if &updated.relative_path == projected {
                projected.clone()
            } else {
                self.unique_remote_path(projected, &op.remote.as_ref().unwrap().id)
                    .await?
            };
            op.move_journal = Some(MoveJournal {
                source: updated.relative_path.clone(),
                target,
                device: fp.device,
                inode: fp.inode,
            });
            self.db.save_operation(op).await?;
        }
        let journal = op.move_journal.as_ref().unwrap();
        let old = local::safe_path(&self.scanner.root, &journal.source)?;
        let destination = local::safe_path(&self.scanner.root, &journal.target)?;
        let installed = match std::fs::symlink_metadata(&destination) {
            Ok(meta) => meta.dev() == journal.device && meta.ino() == journal.inode,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        if !installed {
            let source = local::fingerprint(&old)?;
            if source.device != journal.device || source.inode != journal.inode {
                return Err(Error::Changed);
            }
            std::fs::create_dir_all(destination.parent().unwrap())?;
            if let Err(error) = rename_atomic(&old, &destination, false) {
                if matches!(&error,Error::Io(e) if e.kind() == std::io::ErrorKind::AlreadyExists) {
                    return Err(Error::Changed);
                }
                return Err(error);
            }
        }
        local::sync_directory(old.parent().unwrap())?;
        local::sync_directory(destination.parent().unwrap())?;
        let fp = local::fingerprint(&destination)?;
        if updated.relative_path != journal.target
            || updated.fingerprint.as_ref().map(|old| &old.digest) != Some(&fp.digest)
        {
            updated.generation += 1;
        }
        updated.relative_path = journal.target.clone();
        updated.fingerprint = Some(fp);
        let remote = op.remote.as_ref().unwrap();
        if let Some(base) = &mut updated.baseline {
            base.relative_path = journal.target.clone();
            base.remote_path = Some(projected.clone());
            // Merge metadata only: the old content digest/version must still participate in reconciliation.
            base.remote.name = remote.name.clone();
            base.remote.parents = remote.parents.clone();
        }
        updated.status = crate::db::status(&updated);
        op.stage = "done".into();
        self.db.finish_move(&updated, op).await
    }

    pub async fn recover_moves(&self) -> Result<()> {
        for mut op in self.db.operations().await? {
            if op.kind != "remote_move" {
                continue;
            }
            let mut report = SyncReport::default();
            self.execute_reported(&mut op, &mut report).await?;
            if !report.errors.is_empty() {
                return Err(Error::Invalid(report.errors.join("; ")));
            }
        }
        Ok(())
    }

    async fn install(&self, op: &mut Operation) -> Result<()> {
        let _guard = self.scanner.gate.lock().await;
        self.verify_local_root().await?;
        let mut file = self.db.file(&op.file_id).await?;
        let incoming = op.downloaded.as_ref().unwrap();
        if op.install.is_none() {
            let target = local::safe_path(&self.scanner.root, &file.relative_path)?;
            std::fs::create_dir_all(target.parent().unwrap())?;
            let before = match local::fingerprint(&target) {
                Ok(fp) => Some(fp),
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e),
            };
            if file.generation != op.generation
                || before.as_ref().map(|f| &f.digest)
                    != file.fingerprint.as_ref().map(|f| &f.digest)
            {
                return Err(Error::Changed);
            }
            let staging =
                target
                    .parent()
                    .unwrap()
                    .join(format!("{}{}.swap", local::INTERNAL_PREFIX, op.id));
            let staged = if staging.try_exists()? {
                local::fingerprint(&staging)?
            } else {
                local::durable_copy(&incoming.path, &staging)?
            };
            if staged.digest != incoming.digest {
                return Err(Error::Changed);
            }
            let journal = InstallJournal {
                target,
                staging,
                incoming_inode: staged.inode,
                old_inode: before.as_ref().map(|f| f.inode),
                old_device: before.as_ref().map(|f| f.device),
                old_digest: before.as_ref().map(|f| f.digest.clone()),
                incoming_digest: incoming.digest.clone(),
                exchanged: false,
            };
            op.install = Some(journal);
            self.db.save_operation(op).await?;
        }
        let journal = op.install.as_ref().unwrap();
        if let Some(old_digest) = &journal.old_digest {
            // Register BEFORE exchange. Incoming inode means exchange has not occurred yet.
            self.db
                .displaced(&DisplacedFile {
                    path: journal.staging.clone(),
                    original_relative_path: file.relative_path.clone(),
                    expected: old_digest.clone(),
                    incoming_inode: journal.incoming_inode,
                    promoted_sha256: None,
                })
                .await?;
        }
        let staging_meta = std::fs::symlink_metadata(&journal.staging).ok();
        let already_installed = journal.exchanged
            || match &staging_meta {
                Some(meta) => meta.ino() != journal.incoming_inode,
                None => true, // NOREPLACE installation moved staging; never overwrite the user's replacement on replay.
            };
        if !already_installed {
            if file.generation != op.generation {
                op.stage = "cancelled".into();
                self.db.save_operation(op).await?;
                return Err(Error::Changed);
            }
            let actual = match local::fingerprint(&journal.target) {
                Ok(fp) => Some(fp.digest),
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e),
            };
            if actual != journal.old_digest {
                op.stage = "cancelled".into();
                self.db.save_operation(op).await?;
                return Err(Error::Changed);
            }
            rename_atomic(
                &journal.staging,
                &journal.target,
                journal.old_inode.is_some(),
            )?;
            local::sync_directory(journal.target.parent().unwrap())?;
        }
        op.install.as_mut().unwrap().exchanged = true;
        op.stage = "installed".into();
        self.db.save_operation(op).await?;
        let actual = local::fingerprint(&op.install.as_ref().unwrap().target)?;
        if actual.digest != incoming.digest {
            file.generation += 1;
        }
        file.fingerprint = Some(actual);
        file.materialized = true;
        self.db.save_file(&file).await?;
        let baseline_path = file
            .baseline
            .as_ref()
            .filter(|base| base.relative_path != file.relative_path)
            .map(|base| base.relative_path.clone())
            .unwrap_or_else(|| file.relative_path.clone());
        self.db
            .acknowledge(
                &file.id,
                op.generation,
                incoming.digest.clone(),
                op.remote.clone().unwrap(),
                baseline_path,
                op.projected_path.clone(),
            )
            .await?;
        Ok(())
    }

    pub async fn recover_displaced(&self) -> Result<()> {
        let _guard = self.scanner.gate.lock().await;
        for mut displaced in self.db.displaced_files().await? {
            let fp = local::fingerprint(&displaced.path)?;
            if fp.inode == displaced.incoming_inode {
                continue;
            } // journaled but not exchanged yet
            if fp.digest == displaced.expected
                || displaced.promoted_sha256.as_ref() == Some(&fp.digest.sha256)
            {
                continue;
            }
            let relative = conflict_name(
                &displaced.original_relative_path,
                &format!("Conflict - Recovery-{}", &fp.digest.sha256[..12]),
            );
            let target = local::safe_path(&self.scanner.root, &relative)?;
            if !target.try_exists()? {
                local::durable_copy(&displaced.path, &target)?;
            }
            let recovered = local::fingerprint(&target)?;
            if recovered.digest != fp.digest {
                return Err(Error::Changed);
            }
            self.db.record_local(&relative, Some(recovered)).await?;
            if let Some(mut file) = self
                .db
                .files()
                .await?
                .into_iter()
                .find(|f| f.relative_path == relative)
            {
                file.status = "recovery_review".into();
                file.review_required = true;
                self.db.save_file(&file).await?;
            }
            displaced.promoted_sha256 = Some(fp.digest.sha256);
            self.db.displaced(&displaced).await?;
        }
        Ok(())
    }

    async fn remote_trash(&self, op: &mut Operation, file: &LocalFile) -> Result<()> {
        let _guard = self.scanner.gate.lock().await;
        let source = local::safe_path(&self.scanner.root, &file.relative_path)?;
        if op.conflict_path.is_none() {
            let recovery = Path::new(&file.relative_path).with_file_name(format!(
                "{}{}.removed",
                local::INTERNAL_PREFIX,
                op.id
            ));
            op.conflict_path = Some(recovery.to_string_lossy().into_owned());
            self.db.save_operation(op).await?;
        }
        let recovery = local::safe_path(&self.scanner.root, op.conflict_path.as_ref().unwrap())?;
        let expected = file.baseline.as_ref().ok_or(Error::Changed)?.digest.clone();
        self.db
            .displaced(&DisplacedFile {
                path: recovery.clone(),
                original_relative_path: file.relative_path.clone(),
                expected: expected.clone(),
                incoming_inode: 0,
                promoted_sha256: None,
            })
            .await?;
        if source.try_exists()? {
            if self.db.file(&file.id).await?.generation != op.generation
                || local::fingerprint(&source)?.digest != expected
            {
                return Err(Error::Changed);
            }
            rename_atomic(&source, &recovery, false)?;
            local::sync_directory(source.parent().unwrap())?;
        } else if !recovery.try_exists()? {
            return Err(Error::Changed);
        }
        let mut current = self.db.file(&file.id).await?;
        current.fingerprint = None;
        current.materialized = false;
        current.status = "tombstone".into();
        if let Some(base) = &mut current.baseline {
            base.remote = op.remote.clone().unwrap();
        }
        self.db.save_file(&current).await?;
        op.stage = "done".into();
        self.db.save_operation(op).await
    }
}

pub fn conflict_name(relative: &str, label: &str) -> String {
    let path = Path::new(relative);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let label = truncate_utf8(label, 96);
    let ext = truncate_utf8(&ext, 80);
    let stem_budget = 255usize.saturating_sub(label.len() + ext.len() + 3);
    let stem = if stem.len() > stem_budget {
        format!(
            "{}-{}",
            truncate_utf8(&stem, stem_budget.saturating_sub(13)),
            short_hash(relative)
        )
    } else {
        stem.to_string()
    };
    path.with_file_name(format!("{stem} ({label}){ext}"))
        .to_string_lossy()
        .into_owned()
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn short_hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..12].to_string()
}

fn cursor_error(error: Error) -> Error {
    if matches!(&error, Error::Http { status: 410, .. })
        || matches!(&error,Error::Http {status:400,reason}
        if matches!(reason.as_str(),"invalidPageToken" | "pageTokenExpired" | "invalid"))
    {
        Error::CursorRejected
    } else {
        error
    }
}
fn encode_name(name: &str) -> String {
    let mut value = name
        .replace('%', "%25")
        .replace('/', "%2F")
        .replace('\0', "%00");
    if matches!(value.as_str(), "" | "." | "..") || value.starts_with(local::INTERNAL_PREFIX) {
        value = format!("%{}", short_hash(name));
    }
    if value.len() > 255 {
        value = conflict_name(&value, &format!("Drive-{}", short_hash(name)));
    }
    value
}

pub(crate) fn project_paths(remotes: &[RemoteFile], root: &str) -> HashMap<String, String> {
    // Build indices once. Metadata-only profiles can contain tens of thousands of
    // items: scanning the whole account for every duplicate name is quadratic.
    let mut children: HashMap<&str, Vec<&RemoteFile>> = HashMap::new();
    let mut duplicates: HashMap<(&str, &[String]), usize> = HashMap::new();
    for file in remotes.iter().filter(|f| !f.trashed && !f.unavailable) {
        *duplicates.entry((&file.name, &file.parents)).or_default() += 1;
        if let Some(parent) = file.parents.first() {
            children.entry(parent).or_default().push(file);
        }
    }
    let mut paths = HashMap::from([(root.to_string(), String::new())]);
    let mut queue = std::collections::VecDeque::from([(root.to_string(), 0usize)]);
    while let Some((parent_id, depth)) = queue.pop_front() {
        if depth >= 128 {
            continue;
        }
        let parent_path = paths[&parent_id].clone();
        for file in children.get(parent_id.as_str()).into_iter().flatten() {
            if paths.contains_key(&file.id) {
                continue;
            }
            let name = encode_name(&file.name);
            let name = if duplicates
                .get(&(file.name.as_str(), file.parents.as_slice()))
                .copied()
                .unwrap_or(0)
                > 1
            {
                conflict_name(&name, &format!("Drive-{}", short_hash(&file.id)))
            } else {
                name
            };
            paths.insert(
                file.id.clone(),
                if parent_path.is_empty() {
                    name
                } else {
                    format!("{parent_path}/{name}")
                },
            );
            queue.push_back((file.id.clone(), depth + 1));
        }
    }
    paths.remove(root);
    paths
}

fn retry_delay_ms(error: &Error, attempt: u32, id: &str) -> i64 {
    let retryable = matches!(error, Error::Transport(_) | Error::Io(_))
        || matches!(
            error,
            Error::Http {
                status: 429 | 500..=599,
                ..
            }
        )
        || matches!(error,Error::Http {status:403,reason} if reason == "rateLimitExceeded" || reason == "userRateLimitExceeded");
    if !retryable {
        return 60_000;
    }
    let jitter = id
        .bytes()
        .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64))
        % 1000;
    (1000u64.saturating_mul(1u64 << attempt.min(8)) + jitter).min(300_000) as i64
}

/// Exchange preserves the replaced inode (including any open editor FD) at staging.
#[cfg(target_os = "linux")]
fn rename_atomic(source: &Path, target: &Path, exchange: bool) -> Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| Error::Invalid("NUL in path".into()))?;
    let target = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| Error::Invalid("NUL in path".into()))?;
    // SAFETY: both C strings are owned and valid; flags never allow overwriting without preservation.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            if exchange {
                libc::RENAME_EXCHANGE
            } else {
                libc::RENAME_NOREPLACE
            },
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn rename_atomic(_source: &Path, _target: &Path, _exchange: bool) -> Result<()> {
    Err(Error::Unsupported(
        "safe atomic install currently requires Linux renameat2".into(),
    ))
}
