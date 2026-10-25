use crate::{
    db::Database,
    drive::DriveClient,
    local::{durable_copy, fingerprint, private_dir, sync_directory},
    model::*,
    Error, Result,
};
use futures_util::StreamExt;
use md5::Md5;
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashSet,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock, Semaphore};

// A disconnected/cancelled download must not accumulate unusable partial files.
struct PartialDownload(PathBuf);
impl Drop for PartialDownload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct EvictionReport {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub removed_objects: usize,
    pub protected_bytes: u64,
    pub budget_satisfied: bool,
}

#[derive(Clone)]
pub struct CacheStore {
    pub root: PathBuf,
    pub db: Database,
    gate: Arc<RwLock<()>>,
    objects: Arc<crate::keyed_lock::KeyedLocks>,
    downloads: Arc<Semaphore>,
    pub(crate) policy_gate: Arc<Mutex<()>>,
}
impl CacheStore {
    pub fn new(root: PathBuf, db: Database) -> Result<Self> {
        for name in ["cache", "outbox", "staging", "recovery"] {
            private_dir(&root.join(name))?;
        }
        Ok(Self {
            root,
            db,
            gate: Arc::new(RwLock::new(())),
            objects: Arc::new(crate::keyed_lock::KeyedLocks::default()),
            downloads: Arc::new(Semaphore::new(3)),
            policy_gate: Arc::new(Mutex::new(())),
        })
    }
    pub async fn snapshot(&self, source: &Path) -> Result<Snapshot> {
        let path = self
            .root
            .join("outbox")
            .join(format!("{}.blob", uuid::Uuid::new_v4()));
        let src = source.to_path_buf();
        let out = path.clone();
        let fp = tokio::task::spawn_blocking(move || durable_copy(&src, &out))
            .await
            .map_err(|e| Error::Invalid(format!("snapshot task: {e}")))??;
        Ok(Snapshot {
            path,
            digest: fp.digest,
        })
    }

    pub async fn download_to_cache(
        &self,
        drive: &dyn DriveClient,
        expected: &RemoteFile,
    ) -> Result<Snapshot> {
        if expected.workspace() || expected.folder() {
            return Err(Error::Unsupported(
                "Workspace requires export; folder has no blob".into(),
            ));
        }
        if expected.capabilities.can_download == Some(false) {
            return Err(Error::Unsupported("file is not downloadable".into()));
        }
        if expected.version.is_empty() {
            return Err(Error::Invalid("remote version is required".into()));
        }
        let key = format!(
            "{:x}",
            Sha256::digest(
                format!("{}\0{}\0{}", self.db.account, expected.id, expected.version).as_bytes()
            )
        );
        // One background transfer and up to two explicit opens can progress.
        // Identical immutable objects still have exactly one writer. Pruning
        // holds the exclusive side, so it cannot remove an in-flight object.
        let _object = self.objects.lock(&key).await;
        let _guard = self.gate.read().await;
        let final_path = self.root.join("cache").join(format!("{key}.blob"));
        if final_path.try_exists()? {
            let fp = fingerprint(&final_path)?;
            if expected.matches(&fp.digest) != Some(false) {
                self.verify_version(drive, expected).await?;
                let snapshot = Snapshot {
                    path: final_path,
                    digest: fp.digest,
                };
                self.db.cache(&key, &snapshot, expected).await?;
                return Ok(snapshot);
            }
            return Err(Error::Invalid(
                "cached object checksum mismatch; preserved for inspection".into(),
            ));
        }
        let _permit = self
            .downloads
            .acquire()
            .await
            .expect("download semaphore open");
        let partial = self
            .root
            .join("staging")
            .join(format!("{}.part", uuid::Uuid::new_v4()));
        let out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial)?;
        let _partial = PartialDownload(partial.clone());
        let mut out = tokio::fs::File::from_std(out);
        let mut stream = drive.download(&expected.id).await?;
        let mut sha = Sha256::new();
        let mut md5 = Md5::new();
        let mut size = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            out.write_all(&chunk).await?;
            sha.update(&chunk);
            md5.update(&chunk);
            size += chunk.len() as u64;
        }
        let digest = Digest {
            sha256: format!("{:x}", sha.finalize()),
            md5: format!("{:x}", md5.finalize()),
            size,
        };
        if expected.matches(&digest) == Some(false)
            || expected
                .size
                .as_ref()
                .and_then(|s| s.parse::<u64>().ok())
                .is_some_and(|n| n != size)
        {
            return Err(Error::Invalid("download checksum/size mismatch".into()));
        }
        self.verify_version(drive, expected).await?;
        out.flush().await?;
        out.sync_all().await?;
        drop(out);
        // The engine holds a single-process lock. Cache objects are immutable and never hardlinked writable.
        tokio::fs::rename(&partial, &final_path).await?;
        sync_directory(final_path.parent().unwrap())?;
        let snapshot = Snapshot {
            path: final_path,
            digest,
        };
        self.db.cache(&key, &snapshot, expected).await?;
        Ok(snapshot)
    }

    /// Removes only registered, complete, reproducible cache objects; never touches outbox/recovery.
    pub(crate) async fn prune(&self, max_bytes: u64) -> Result<EvictionReport> {
        self.prune_selected(max_bytes, None).await
    }

    pub(crate) async fn remove_for_remote_ids(&self, ids: &HashSet<String>) -> Result<()> {
        if !ids.is_empty() {
            self.prune_selected(0, Some(ids)).await?;
        }
        Ok(())
    }

    async fn prune_selected(
        &self,
        max_bytes: u64,
        ids: Option<&HashSet<String>>,
    ) -> Result<EvictionReport> {
        let _guard = self.gate.write().await;
        let _policy = self.policy_gate.lock().await;
        let directory = self.root.join("cache");
        if !std::fs::symlink_metadata(&directory)?.is_dir() {
            return Err(Error::Invalid(
                "cache directory is not a regular directory".into(),
            ));
        }
        let files = self.db.files().await?;
        let mut protected_ids: HashSet<_> = files
            .iter()
            .filter(|file| {
                file.review_required || !matches!(file.status.as_str(), "synced" | "online_only")
            })
            .filter_map(|file| file.remote_id.clone())
            .collect();
        protected_ids.extend(
            crate::offline::Graph::load(&self.db, false)
                .await?
                .protected_remote_ids(),
        );
        let mut protected_paths = HashSet::new();
        for op in self.db.operations().await? {
            for snapshot in [op.downloaded, op.snapshot].into_iter().flatten() {
                protected_paths.insert(snapshot.path);
            }
        }
        let mut report = EvictionReport::default();
        let mut candidates = Vec::new();
        for object in self.db.cached_objects().await? {
            if object.key.len() != 64
                || !object.key.bytes().all(|b| b.is_ascii_hexdigit())
                || object.snapshot.path != directory.join(format!("{}.blob", object.key))
            {
                return Err(Error::Invalid(
                    "cache record points outside its registered object path".into(),
                ));
            }
            let meta = match std::fs::symlink_metadata(&object.snapshot.path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.db.forget_cache(&object.key).await?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if !meta.is_file() {
                return Err(Error::Invalid("cache object is not a regular file".into()));
            }
            let size = meta.len();
            report.before_bytes = report.before_bytes.saturating_add(size);
            if object
                .remote_id
                .as_ref()
                .is_none_or(|id| protected_ids.contains(id))
                || ids
                    .is_some_and(|ids| object.remote_id.as_ref().is_none_or(|id| !ids.contains(id)))
                || protected_paths.contains(&object.snapshot.path)
            {
                report.protected_bytes = report.protected_bytes.saturating_add(size);
            } else {
                candidates.push((object, size));
            }
        }
        report.after_bytes = report.before_bytes;
        for (object, size) in candidates {
            if report.after_bytes <= max_bytes {
                break;
            }
            std::fs::remove_file(&object.snapshot.path)?;
            sync_directory(&directory)?;
            self.db.forget_cache(&object.key).await?;
            report.after_bytes = report.after_bytes.saturating_sub(size);
            report.removed_objects += 1;
        }
        report.budget_satisfied = report.after_bytes <= max_bytes;
        Ok(report)
    }
    async fn verify_version(&self, drive: &dyn DriveClient, expected: &RemoteFile) -> Result<()> {
        let current = drive.get(&expected.id).await?.ok_or(Error::Changed)?;
        if current.version != expected.version || current.trashed || current.unavailable {
            return Err(Error::Changed);
        }
        Ok(())
    }
}
