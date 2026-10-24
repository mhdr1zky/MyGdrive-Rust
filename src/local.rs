use crate::{db::Database, model::*, Error, Result};
use md5::Md5;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

pub const INTERNAL_PREFIX: &str = ".mygdrive-internal-";

pub fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(Error::Invalid("private directory is a symlink".into()));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn safe_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let rel = Path::new(relative);
    if rel.as_os_str().is_empty() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(Error::Invalid(
            "expected a nonempty relative path without traversal".into(),
        ));
    }
    let mut path = root.to_path_buf();
    for part in rel.components() {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Error::Invalid("symlink in sync path".into()))
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}

fn unchanged_fingerprint(path: &Path, previous: &Fingerprint) -> Result<Option<Fingerprint>> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(Error::Unsupported(
            "only regular files with one hard link are supported".into(),
        ));
    }
    let modified = meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128;
    let changed = meta.ctime() as i128 * 1_000_000_000 + meta.ctime_nsec() as i128;
    Ok((meta.dev() == previous.device
        && meta.ino() == previous.inode
        && meta.len() == previous.digest.size
        && modified == previous.modified_ns
        && changed == previous.changed_ns)
        .then(|| previous.clone()))
}

pub fn fingerprint(path: &Path) -> Result<Fingerprint> {
    fingerprint_copy(path, None)
}

fn stamp(meta: &std::fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

pub fn fingerprint_copy(path: &Path, mut destination: Option<&mut File>) -> Result<Fingerprint> {
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let before = source.metadata()?;
    if !before.is_file() || before.nlink() != 1 {
        return Err(Error::Unsupported(
            "only regular files with one hard link are supported".into(),
        ));
    }
    let mut sha = Sha256::new();
    let mut md5 = Md5::new();
    let mut buf = [0; 128 * 1024];
    let mut count = 0u64;
    loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
        md5.update(&buf[..n]);
        count += n as u64;
        if let Some(out) = destination.as_deref_mut() {
            out.write_all(&buf[..n])?;
        }
    }
    if stamp(&before) != stamp(&source.metadata()?)
        || stamp(&before) != stamp(&std::fs::symlink_metadata(path)?)
        || count != before.len()
    {
        return Err(Error::Changed);
    }
    Ok(Fingerprint {
        digest: Digest {
            sha256: format!("{:x}", sha.finalize()),
            md5: format!("{:x}", md5.finalize()),
            size: count,
        },
        device: before.dev(),
        inode: before.ino(),
        modified_ns: before.mtime() as i128 * 1_000_000_000 + before.mtime_nsec() as i128,
        changed_ns: before.ctime() as i128 * 1_000_000_000 + before.ctime_nsec() as i128,
    })
}

pub fn durable_copy(source: &Path, destination: &Path) -> Result<Fingerprint> {
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)?;
    let fp = fingerprint_copy(source, Some(&mut out))?;
    out.sync_all()?;
    sync_directory(
        destination
            .parent()
            .ok_or_else(|| Error::Invalid("missing parent".into()))?,
    )?;
    let written = fingerprint(destination)?;
    if written.digest != fp.digest {
        return Err(Error::Changed);
    }
    Ok(written)
}
pub fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[derive(Clone)]
pub struct Scanner {
    pub root: PathBuf,
    pub db: Database,
    pub gate: Arc<Mutex<()>>,
}

impl Scanner {
    pub async fn scan(&self) -> Result<()> {
        let guard = self.gate.lock().await;
        if self
            .db
            .operations()
            .await?
            .iter()
            .any(|op| op.kind == "remote_move" && op.move_journal.is_some())
        {
            return Err(Error::Changed);
        }
        let root = self.root.clone();
        let meta = std::fs::metadata(&root)?;
        self.db
            .bind("root_identity", &format!("{}:{}", meta.dev(), meta.ino()))
            .await?;
        crate::directory_sync::recover(&self.db, &self.root).await?;
        let files = self.db.materialized_files().await?;
        let previous: HashMap<String, Fingerprint> = files
            .iter()
            .filter_map(|f| {
                f.fingerprint
                    .clone()
                    .map(|fp| (f.relative_path.clone(), fp))
            })
            .collect();
        // Hashing a changed large file must not hold the namespace/scanner gate.
        drop(guard);
        let observed =
            tokio::task::spawn_blocking(move || -> Result<BTreeMap<String, Fingerprint>> {
                let mut paths = BTreeMap::new();
                // No tombstones are written unless the entire traversal succeeds.
                for item in walkdir::WalkDir::new(&root)
                    .follow_links(false)
                    .into_iter()
                    .filter_entry(|e| !e.file_name().to_string_lossy().starts_with(INTERNAL_PREFIX))
                {
                    let item = item.map_err(|e| Error::Invalid(format!("scan incomplete: {e}")))?;
                    if item.path() == root {
                        continue;
                    }
                    if item.file_type().is_symlink() {
                        return Err(Error::Unsupported("symlink in sync tree".into()));
                    }
                    if item.file_type().is_dir() {
                        continue;
                    }
                    let relative = item
                        .path()
                        .strip_prefix(&root)
                        .map_err(|_| Error::Invalid("outside root".into()))?
                        .to_str()
                        .ok_or_else(|| Error::Unsupported("non UTF-8 path".into()))?
                        .to_string();
                    let cached = previous
                        .get(&relative)
                        .map(|fp| unchanged_fingerprint(item.path(), fp))
                        .transpose()?
                        .flatten();
                    paths.insert(
                        relative,
                        if let Some(fp) = cached {
                            fp
                        } else {
                            fingerprint(item.path())?
                        },
                    );
                }
                Ok(paths)
            })
            .await
            .map_err(|e| Error::Invalid(format!("scan task: {e}")))??;
        let _guard = self.gate.lock().await;
        let current_root = std::fs::metadata(&self.root)?;
        if current_root.dev() != meta.dev() || current_root.ino() != meta.ino() {
            return Err(Error::Changed);
        }
        // Revalidate after re-acquiring the gate: an install or editor may have
        // changed a path while hashing. An incomplete/stale scan never deletes rows.
        for (path, fp) in &observed {
            if unchanged_fingerprint(&safe_path(&self.root, path)?, fp)?.is_none() {
                return Err(Error::Changed);
            }
        }
        for file in &files {
            if !observed.contains_key(&file.relative_path)
                && safe_path(&self.root, &file.relative_path)?.try_exists()?
            {
                return Err(Error::Changed);
            }
        }
        let observed_paths: Vec<_> = observed.keys().cloned().collect();
        let occupied = self.db.occupied_file_paths(&observed_paths).await?;
        let mut renamed = HashSet::new();
        // An exact inode match preserves identity through normal moves, including directory moves.
        for file in &files {
            if observed.contains_key(&file.relative_path) {
                continue;
            }
            if let Some(old) = &file.fingerprint {
                let candidates: Vec<_> = observed
                    .iter()
                    .filter(|(path, fp)| {
                        fp.device == old.device
                            && fp.inode == old.inode
                            && !occupied.contains(*path)
                    })
                    .collect();
                if candidates.len() == 1 {
                    let (path, fp) = candidates[0];
                    let mut moved = self.db.file(&file.id).await?;
                    moved.relative_path = path.clone();
                    moved.fingerprint = Some(fp.clone());
                    moved.generation += 1;
                    moved.status = crate::db::status(&moved);
                    self.db.save_file(&moved).await?;
                    renamed.insert(file.id.clone());
                }
            }
        }
        for (path, fp) in &observed {
            self.db.record_local(path, Some(fp.clone())).await?;
        }
        for file in &files {
            if file.materialized
                && !observed.contains_key(&file.relative_path)
                && !renamed.contains(&file.id)
            {
                self.db.record_local(&file.relative_path, None).await?;
            }
        }
        crate::directory_sync::scan(&self.db, &self.root).await?;
        Ok(())
    }
}

pub struct LocalWatcher {
    _watcher: notify::RecommendedWatcher,
    rx: mpsc::Receiver<()>,
    rescan: Arc<AtomicBool>,
}
impl LocalWatcher {
    pub fn start(root: &Path) -> Result<Self> {
        let (tx, rx) = mpsc::channel(64);
        let rescan = Arc::new(AtomicBool::new(false));
        let flag = rescan.clone();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            match event {
                Ok(event) => {
                    if matches!(event.kind, EventKind::Access(_)) {
                        return;
                    }
                    if !event.paths.is_empty()
                        && event.paths.iter().all(|p| {
                            p.components().any(|c| {
                                c.as_os_str().to_string_lossy().starts_with(INTERNAL_PREFIX)
                            })
                        })
                    {
                        return;
                    }
                    if event.need_rescan() {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
                Err(_) => flag.store(true, Ordering::Relaxed),
            }
            if tx.try_send(()).is_err() {
                flag.store(true, Ordering::Relaxed);
            }
        })?;
        watcher.watch(root, RecursiveMode::Recursive)?;
        Ok(Self {
            _watcher: watcher,
            rx,
            rescan,
        })
    }
    pub async fn run(self, scanner: Scanner, stop: CancellationToken) -> Result<()> {
        self.run_inner(scanner, stop, None).await
    }
    pub(crate) async fn run_notifying(
        self,
        scanner: Scanner,
        stop: CancellationToken,
        changes: mpsc::Sender<()>,
    ) -> Result<()> {
        self.run_inner(scanner, stop, Some(changes)).await
    }
    pub(crate) async fn pending_changes(scanner: &Scanner) -> Result<HashSet<(String, i64)>> {
        // Only dirty generations wake sync. Reads, repeated scans and clean
        // engine installs must not create a self-sustaining polling loop.
        let files: Vec<(String, i64)> = sqlx::query_as("SELECT id,generation FROM files WHERE sync_status IN ('new_local','modified_local','deleted_local') AND coalesce(json_extract(data,'$.review_required'),0)=0")
            .fetch_all(&scanner.db.pool).await?;
        let mut current: HashSet<_> = files
            .into_iter()
            .map(|(id, generation)| (format!("f:{id}"), generation))
            .collect();
        for dir in crate::directory_sync::directories(&scanner.db).await? {
            if !dir.review_required
                && dir.move_journal.is_none()
                && match &dir.baseline {
                    Some(base) => !dir.present || dir.relative_path != base.relative_path,
                    None => dir.present,
                }
            {
                current.insert((format!("d:{}", dir.id), dir.generation));
            }
        }
        Ok(current)
    }
    async fn notify_changes(
        scanner: &Scanner,
        previous: &mut HashSet<(String, i64)>,
        changes: &mpsc::Sender<()>,
    ) -> Result<()> {
        let current = Self::pending_changes(scanner).await?;
        if current.difference(previous).next().is_some() {
            // Capacity one coalesces save bursts while a cycle is active.
            let _ = changes.try_send(());
        }
        *previous = current;
        Ok(())
    }
    async fn run_inner(
        mut self,
        scanner: Scanner,
        stop: CancellationToken,
        changes: Option<mpsc::Sender<()>>,
    ) -> Result<()> {
        // The watcher already exists before this scan, so events during traversal are buffered.
        scanner.scan().await?;
        let mut previous = HashSet::new();
        if let Some(changes) = &changes {
            Self::notify_changes(&scanner, &mut previous, changes).await?;
        }
        let mut periodic = tokio::time::interval(Duration::from_secs(30));
        periodic.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                event = self.rx.recv() => {
                    if event.is_none() { return Ok(()); }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    while self.rx.try_recv().is_ok() {}
                },
                _ = periodic.tick() => {},
            }
            self.rescan.swap(false, Ordering::Relaxed);
            if let Err(e) = scanner.scan().await {
                tracing::warn!(error=%e,"local scan deferred; no deletion inferred from incomplete scan");
            } else if let Some(changes) = &changes {
                if let Err(error) = Self::notify_changes(&scanner, &mut previous, changes).await {
                    tracing::warn!(%error, "local sync wake deferred");
                }
            }
        }
    }
}
