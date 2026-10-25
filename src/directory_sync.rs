//! Durable identity tracking for *materialized* directories. Cloud-only directory
//! absence is never a deletion. Scanner owns the filesystem gate when observing
//! or recovering a move; network operations deliberately do not hold that gate.
use crate::{
    db::Database,
    drive::DriveClient,
    local::{self, Scanner},
    model::{LocalFile, RemoteFile},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    os::unix::fs::MetadataExt,
    path::Path,
};

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS materialized_directories(id TEXT PRIMARY KEY,relative_path TEXT NOT NULL,remote_id TEXT UNIQUE,data TEXT NOT NULL); CREATE INDEX IF NOT EXISTS materialized_directory_path ON materialized_directories(relative_path);";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectoryBaseline {
    pub relative_path: String,
    pub remote: RemoteFile,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectoryMove {
    pub source: String,
    pub target: String,
    pub remote: RemoteFile,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Directory {
    pub id: String,
    pub relative_path: String,
    pub device: u64,
    pub inode: u64,
    pub present: bool,
    pub generation: i64,
    pub remote_id: Option<String>,
    pub baseline: Option<DirectoryBaseline>,
    #[serde(default)]
    pub creation_path: Option<String>,
    #[serde(default)]
    pub creation_parent: Option<String>,
    pub review_required: bool,
    pub move_journal: Option<DirectoryMove>,
}

pub async fn directories(db: &Database) -> Result<Vec<Directory>> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT data FROM materialized_directories")
        .fetch_all(&db.pool)
        .await?;
    rows.into_iter()
        .map(|row| Ok(serde_json::from_str(&row)?))
        .collect()
}
async fn save(db: &Database, dir: &Directory) -> Result<()> {
    sqlx::query("INSERT INTO materialized_directories(id,relative_path,remote_id,data) VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET relative_path=excluded.relative_path,remote_id=excluded.remote_id,data=excluded.data WHERE materialized_directories.data IS NOT excluded.data")
        .bind(&dir.id).bind(&dir.relative_path).bind(&dir.remote_id).bind(serde_json::to_string(dir)?)
        .execute(&db.pool).await?;
    Ok(())
}
async fn latest(db: &Database, id: &str) -> Result<Directory> {
    let data: String = sqlx::query_scalar("SELECT data FROM materialized_directories WHERE id=?")
        .bind(id)
        .fetch_one(&db.pool)
        .await?;
    Ok(serde_json::from_str(&data)?)
}
fn under(path: &str, ancestor: &str) -> bool {
    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|tail| tail.starts_with('/'))
}
fn rebase(path: &str, source: &str, target: &str) -> String {
    format!("{target}{}", &path[source.len()..])
}

#[derive(Default)]
struct ScanIndex {
    inodes: HashMap<(u64, u64), BTreeSet<usize>>,
    paths: HashMap<String, BTreeSet<usize>>,
}
impl ScanIndex {
    fn new(directories: &[Directory]) -> Self {
        let mut index = Self::default();
        for (position, directory) in directories.iter().enumerate() {
            index.insert(position, directory);
        }
        index
    }
    fn insert(&mut self, position: usize, directory: &Directory) {
        self.inodes
            .entry((directory.device, directory.inode))
            .or_default()
            .insert(position);
        self.paths
            .entry(directory.relative_path.clone())
            .or_default()
            .insert(position);
    }
    fn find(
        &self,
        directories: &[Directory],
        seen: &HashSet<String>,
        path: &str,
        device: u64,
        inode: u64,
    ) -> Option<usize> {
        // Keep the old vector's first-match ordering, including an inode match
        // already seen at another path. Only path fallback excludes seen IDs.
        self.inodes
            .get(&(device, inode))
            .and_then(|positions| positions.first().copied())
            .or_else(|| {
                self.paths
                    .get(path)?
                    .iter()
                    .copied()
                    .find(|position| !seen.contains(&directories[*position].id))
            })
    }
    fn replace(&mut self, position: usize, old: &Directory, new: &Directory) {
        if (old.device, old.inode) != (new.device, new.inode) {
            self.inodes
                .get_mut(&(old.device, old.inode))
                .unwrap()
                .remove(&position);
            self.inodes
                .entry((new.device, new.inode))
                .or_default()
                .insert(position);
        }
        if old.relative_path != new.relative_path {
            self.paths
                .get_mut(&old.relative_path)
                .unwrap()
                .remove(&position);
            self.paths
                .entry(new.relative_path.clone())
                .or_default()
                .insert(position);
        }
    }
}

/// Only called for parents from an in-scope projected cloud path or a durable
/// move journal. Each mkdir and its directory entry are flushed independently,
/// making a restart midway through a deep parent chain safe to repeat.
fn durable_move_parents(root: &Path, relative_target: &str) -> Result<()> {
    let relative = Path::new(relative_target).parent().unwrap_or(Path::new(""));
    let mut current = std::path::PathBuf::new();
    for component in relative.components() {
        current.push(component);
        let relative = current
            .to_str()
            .ok_or_else(|| Error::Unsupported("non UTF-8 directory".into()))?;
        let path = local::safe_path(root, relative)?;
        match std::fs::create_dir(&path) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !std::fs::symlink_metadata(&path)?.is_dir() {
                    return Err(Error::Changed);
                }
            }
            Err(error) => return Err(error.into()),
        }
        // Repeat these flushes even when mkdir happened before a crash.
        local::sync_directory(&path)?;
        local::sync_directory(path.parent().unwrap())?;
    }
    Ok(())
}

/// Called while Scanner.gate is held, before publishing any new observation.
pub async fn scan(db: &Database, root: &Path) -> Result<()> {
    recover(db, root).await?;
    let root_owned = root.to_path_buf();
    let observed = tokio::task::spawn_blocking(move || -> Result<Vec<(String, u64, u64)>> {
        let mut observed = Vec::new();
        for entry in walkdir::WalkDir::new(&root_owned)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                !e.file_name()
                    .to_string_lossy()
                    .starts_with(local::INTERNAL_PREFIX)
            })
        {
            let entry =
                entry.map_err(|e| Error::Invalid(format!("directory scan incomplete: {e}")))?;
            if entry.path() == root_owned || !entry.file_type().is_dir() {
                continue;
            }
            let metadata = entry
                .metadata()
                .map_err(|e| Error::Invalid(format!("directory metadata: {e}")))?;
            let path = entry
                .path()
                .strip_prefix(&root_owned)
                .map_err(|_| Error::Changed)?
                .to_str()
                .ok_or_else(|| Error::Unsupported("non UTF-8 directory".into()))?
                .to_string();
            observed.push((path, metadata.dev(), metadata.ino()));
        }
        Ok(observed)
    })
    .await
    .map_err(|e| Error::Invalid(format!("directory scan: {e}")))??;
    let mut known = directories(db).await?;
    let mut lookup = ScanIndex::new(&known);
    let mut seen = HashSet::new();
    // Inode first identifies moves. Path fallback permits delete/recreate at the
    // same location without inventing a second cloud folder with the same name.
    for (path, dev, ino) in observed {
        let index = lookup.find(&known, &seen, &path, dev, ino);
        let old_data = index
            .map(|i| serde_json::to_string(&known[i]))
            .transpose()?;
        let mut directory = if let Some(index) = index {
            known[index].clone()
        } else {
            Directory {
                id: uuid::Uuid::new_v4().to_string(),
                relative_path: path.clone(),
                device: dev,
                inode: ino,
                present: true,
                generation: 0,
                remote_id: None,
                baseline: None,
                creation_path: None,
                creation_parent: None,
                review_required: false,
                move_journal: None,
            }
        };
        if directory
            .baseline
            .as_ref()
            .is_some_and(|b| b.remote.trashed)
            && !directory.present
        {
            directory.remote_id = None;
            directory.baseline = None;
        }
        if directory.relative_path != path
            || !directory.present
            || directory.device != dev
            || directory.inode != ino
        {
            directory.generation += 1;
        }
        directory.relative_path = path;
        directory.device = dev;
        directory.inode = ino;
        directory.present = true;
        seen.insert(directory.id.clone());
        if old_data.as_deref() != Some(serde_json::to_string(&directory)?.as_str()) {
            save(db, &directory).await?;
        }
        if let Some(index) = index {
            lookup.replace(index, &known[index], &directory);
            known[index] = directory;
        }
    }
    for mut directory in known {
        if directory.present && !seen.contains(&directory.id) {
            directory.present = false;
            directory.generation += 1;
            save(db, &directory).await?;
        }
    }
    Ok(())
}

/// Merge an inherited folder move into file baselines without erasing concurrent
/// edits: digests, remote versions and fingerprints remain untouched.
async fn rebase_descendants(
    db: &Database,
    source: &str,
    target: &str,
    move_local: bool,
) -> Result<()> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT data FROM files WHERE relative_path=? OR substr(relative_path,1,?)=? OR relative_path=? OR substr(relative_path,1,?)=? OR json_extract(data,'$.baseline.relative_path')=? OR substr(json_extract(data,'$.baseline.relative_path'),1,?)=?")
        .bind(source).bind(source.chars().count() as i64 + 1).bind(format!("{source}/"))
        .bind(target).bind(target.chars().count() as i64 + 1).bind(format!("{target}/"))
        .bind(source).bind(source.chars().count() as i64 + 1).bind(format!("{source}/"))
        .fetch_all(&db.pool).await?;
    let mut files = Vec::new();
    for row in rows {
        let mut file: LocalFile = serde_json::from_str(&row)?;
        let old = file.relative_path.clone();
        if move_local && under(&old, source) {
            file.relative_path = rebase(&old, source, target);
            file.generation += 1;
        }
        if let Some(base) = &mut file.baseline {
            // Merge the inherited parent move, retaining a child's independent
            // rename delta (baseline a/x -> b/x while local remains b/y).
            if under(&base.relative_path, source) {
                base.relative_path = rebase(&base.relative_path, source, target);
                if let Some(path) = &mut base.remote_path {
                    if under(path, source) {
                        *path = rebase(path, source, target);
                    }
                }
            }
        }
        file.status = crate::db::status(&file);
        files.push(file);
    }
    let dirs = directories(db).await?;
    let mut tx = db.pool.begin().await?;
    // Vacate unique local file paths before swapping an entire subtree.
    if move_local {
        for file in &files {
            sqlx::query("UPDATE files SET relative_path=? WHERE id=?")
                .bind(format!("{}move-{}", local::INTERNAL_PREFIX, file.id))
                .bind(&file.id)
                .execute(&mut *tx)
                .await?;
        }
    }
    for file in files {
        sqlx::query(
            "UPDATE files SET relative_path=?,generation=?,sync_status=?,data=? WHERE id=?",
        )
        .bind(&file.relative_path)
        .bind(file.generation)
        .bind(&file.status)
        .bind(serde_json::to_string(&file)?)
        .bind(&file.id)
        .execute(&mut *tx)
        .await?;
    }
    for mut dir in dirs {
        if move_local && under(&dir.relative_path, source) {
            dir.relative_path = rebase(&dir.relative_path, source, target);
            dir.generation += 1;
        }
        if let Some(base) = &mut dir.baseline {
            if under(&base.relative_path, source) {
                base.relative_path = rebase(&base.relative_path, source, target);
            }
        }
        sqlx::query("UPDATE materialized_directories SET relative_path=?,data=? WHERE id=?")
            .bind(&dir.relative_path)
            .bind(serde_json::to_string(&dir)?)
            .bind(&dir.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Recover before both file and directory scans: otherwise file scanner could
/// interpret an interrupted whole-folder move as a fresh delete/create.
pub async fn recover(db: &Database, root: &Path) -> Result<()> {
    for directory in directories(db).await? {
        let Some(journal) = &directory.move_journal else {
            continue;
        };
        let source = local::safe_path(root, &journal.source)?;
        let target = local::safe_path(root, &journal.target)?;
        let identity_matches = |path: &Path| -> Result<bool> {
            match std::fs::symlink_metadata(path) {
                Ok(m) => {
                    Ok(m.is_dir() && m.dev() == directory.device && m.ino() == directory.inode)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e.into()),
            }
        };
        if !identity_matches(&target)? {
            if !identity_matches(&source)? || target.try_exists()? {
                return Err(Error::Changed);
            }
            durable_move_parents(root, &journal.target)?;
            let from = std::ffi::CString::new(source.as_os_str().as_encoded_bytes())
                .map_err(|_| Error::Changed)?;
            let to = std::ffi::CString::new(target.as_os_str().as_encoded_bytes())
                .map_err(|_| Error::Changed)?;
            let result = unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    from.as_ptr(),
                    libc::AT_FDCWD,
                    to.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if result != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        local::sync_directory(source.parent().unwrap())?;
        local::sync_directory(target.parent().unwrap())?;
        rebase_descendants(db, &journal.source, &journal.target, true).await?;
        let mut current = latest(db, &directory.id).await?;
        current.relative_path = journal.target.clone();
        current.baseline = Some(DirectoryBaseline {
            relative_path: journal.target.clone(),
            remote: journal.remote.clone(),
        });
        current.move_journal = None;
        save(db, &current).await?;
    }
    Ok(())
}

async fn commit_remote_unlocked(
    db: &Database,
    id: &str,
    path: &str,
    remote: RemoteFile,
) -> Result<()> {
    db.remote_page(
        std::slice::from_ref(&remote),
        "directory_committed",
        &remote.id,
    )
    .await?;
    let mut current = latest(db, id).await?;
    current.remote_id = Some(remote.id.clone());
    current.baseline = Some(DirectoryBaseline {
        relative_path: path.into(),
        remote,
    });
    current.review_required = false;
    save(db, &current).await
}

async fn commit_remote(
    db: &Database,
    scanner: &Scanner,
    id: &str,
    path: &str,
    remote: RemoteFile,
) -> Result<()> {
    let _guard = scanner.gate.lock().await;
    commit_remote_unlocked(db, id, path, remote).await
}

fn parent_path(path: &str) -> &str {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("")
}
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Caller holds Scanner.gate. Pinning a folder also creates empty descendants
/// and records their existing Drive identities before a watcher can see them.
pub(crate) async fn materialize_pinned(
    db: &Database,
    root: &Path,
    remotes: &[RemoteFile],
    paths: &HashMap<String, String>,
    selected: &HashSet<String>,
) -> Result<()> {
    let mut needed = HashSet::new();
    for id in selected {
        if let Some(path) = paths.get(id) {
            let mut path = path.as_str();
            while !path.is_empty() {
                needed.insert(path.to_owned());
                path = parent_path(path);
            }
        }
    }
    let mut folders: Vec<_> = remotes
        .iter()
        .filter_map(|remote| {
            let path = paths.get(&remote.id)?;
            (remote.folder() && needed.contains(path)).then_some((path, remote))
        })
        .collect();
    folders.sort_by_key(|(path, _)| path.matches('/').count());
    let mut known = directories(db).await?;
    let mut blocked = Vec::<String>::new();
    for (path, remote) in folders {
        if blocked.iter().any(|ancestor| under(path, ancestor)) {
            continue;
        }
        let index = known
            .iter()
            .position(|dir| dir.remote_id.as_deref() == Some(&remote.id))
            .or_else(|| known.iter().position(|dir| dir.relative_path == *path));
        if index.is_some_and(|i| {
            let dir = &known[i];
            !dir.present
                || dir.review_required
                || dir.move_journal.is_some()
                || dir.relative_path != *path
                || dir.remote_id.as_ref().is_some_and(|id| id != &remote.id)
        }) {
            blocked.push(path.clone());
            continue;
        }
        let target = local::safe_path(root, path)?;
        match std::fs::create_dir(&target) {
            Ok(()) => {
                local::sync_directory(&target)?;
                local::sync_directory(target.parent().ok_or(Error::Changed)?)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
        let meta = std::fs::symlink_metadata(&target)?;
        if !meta.is_dir() {
            return Err(Error::Changed);
        }
        let mut directory = index
            .map(|i| known[i].clone())
            .unwrap_or_else(|| Directory {
                id: uuid::Uuid::new_v4().to_string(),
                relative_path: path.clone(),
                device: meta.dev(),
                inode: meta.ino(),
                present: true,
                generation: 0,
                remote_id: None,
                baseline: None,
                creation_path: None,
                creation_parent: None,
                review_required: false,
                move_journal: None,
            });
        directory.device = meta.dev();
        directory.inode = meta.ino();
        directory.remote_id = Some(remote.id.clone());
        directory.baseline = Some(DirectoryBaseline {
            relative_path: path.clone(),
            remote: remote.clone(),
        });
        save(db, &directory).await?;
        if let Some(i) = index {
            known[i] = directory;
        } else {
            known.push(directory);
        }
    }
    Ok(())
}

pub async fn reconcile_before(
    db: &Database,
    scanner: &Scanner,
    drive: &dyn DriveClient,
    root_id: &str,
) -> Result<Vec<String>> {
    let mut dirs = directories(db).await?;
    if dirs.is_empty() {
        return Ok(Vec::new());
    }
    let remotes = db.remotes().await?;
    let paths = crate::engine::project_paths(&remotes, root_id);
    let mut path_ids: HashMap<String, String> = remotes
        .iter()
        .filter(|r| r.folder() && !r.trashed && !r.unavailable)
        .filter_map(|r| paths.get(&r.id).map(|p| (p.clone(), r.id.clone())))
        .collect();
    path_ids.insert(String::new(), root_id.into());
    let by_id: HashMap<_, _> = remotes.iter().map(|r| (r.id.as_str(), r)).collect();
    dirs.sort_by_key(|d| d.relative_path.matches('/').count());
    let mut bound: HashSet<String> = dirs.iter().filter_map(|d| d.remote_id.clone()).collect();
    let mut errors = Vec::new();
    for observed in dirs.into_iter().filter(|d| d.present && !d.review_required) {
        let result: Result<()> = async {
            // Re-read local intent immediately before scheduling its mutation.
            let mut dir = latest(db, &observed.id).await?;
            if !dir.present {
                return Ok(());
            }
            let snapshot_path = dir.relative_path.clone();
            let path = local::safe_path(&scanner.root, &snapshot_path)?;
            let metadata = std::fs::symlink_metadata(path)?;
            if !metadata.is_dir() || metadata.dev() != dir.device || metadata.ino() != dir.inode {
                return Err(Error::Changed);
            }
            if dir.remote_id.is_none() {
                if let Some(id) = path_ids
                    .get(&snapshot_path)
                    .filter(|id| !bound.contains(*id))
                    .cloned()
                {
                    if let Some(remote) = by_id.get(id.as_str()) {
                        commit_remote(db, scanner, &dir.id, &snapshot_path, (*remote).clone())
                            .await?;
                        bound.insert(id);
                        return Ok(());
                    }
                }
                // Reserve and persist an ID *before* network creation. Replay
                // fetches this same ID, including after a lost create response.
                let reserved = drive.reserve_id().await?;
                {
                    let _guard = scanner.gate.lock().await;
                    dir = latest(db, &dir.id).await?;
                    if dir.remote_id.is_none() {
                        dir.remote_id = Some(reserved);
                        dir.creation_path = Some(snapshot_path.clone());
                        dir.creation_parent = path_ids.get(parent_path(&snapshot_path)).cloned();
                    }
                    save(db, &dir).await?;
                }
                bound.insert(dir.remote_id.clone().unwrap());
            }
            let id = dir.remote_id.as_deref().unwrap();
            let mut current = if dir
                .baseline
                .as_ref()
                .is_some_and(|base| base.relative_path == snapshot_path)
            {
                by_id.get(id).map(|remote| (*remote).clone())
            } else {
                drive.get(id).await?
            };
            let parent = path_ids
                .get(parent_path(&snapshot_path))
                .ok_or_else(|| Error::Invalid("folder parent not reconciled".into()))?
                .clone();
            if dir.baseline.is_none() {
                let creation_path = dir
                    .creation_path
                    .clone()
                    .unwrap_or_else(|| snapshot_path.clone());
                let creation_parent = dir
                    .creation_parent
                    .clone()
                    .or_else(|| path_ids.get(parent_path(&creation_path)).cloned())
                    .unwrap_or_else(|| parent.clone());
                let remote = match current {
                    Some(remote)
                        if remote.folder()
                            && !remote.trashed
                            && !remote.unavailable
                            && remote.name == basename(&creation_path)
                            && remote.parents == [creation_parent.clone()] =>
                    {
                        remote
                    }
                    Some(_) => return Err(Error::Precondition),
                    None => {
                        drive
                            .create_folder(id, basename(&creation_path), &creation_parent)
                            .await?
                    }
                };
                path_ids.insert(creation_path.clone(), remote.id.clone());
                commit_remote(db, scanner, &dir.id, &creation_path, remote.clone()).await?;
                if creation_path == snapshot_path {
                    return Ok(());
                }
                dir.baseline = Some(DirectoryBaseline {
                    relative_path: creation_path,
                    remote: remote.clone(),
                });
                current = Some(remote);
            }
            let base = dir.baseline.as_ref().unwrap();
            let Some(remote) = current else {
                return Err(Error::Precondition);
            };
            if remote.unavailable || !remote.folder() {
                return Err(Error::Precondition);
            }
            if remote.trashed {
                let _guard = scanner.gate.lock().await;
                let mut current = latest(db, &dir.id).await?;
                if current.generation != dir.generation || snapshot_path != base.relative_path {
                    return Err(Error::Precondition);
                }
                let path = local::safe_path(&scanner.root, &current.relative_path)?;
                let metadata = std::fs::symlink_metadata(&path)?;
                if !metadata.is_dir()
                    || metadata.dev() != current.device
                    || metadata.ino() != current.inode
                {
                    return Err(Error::Changed);
                }
                if std::fs::read_dir(&path)?.next().is_some() {
                    return Err(Error::Changed);
                }
                std::fs::remove_dir(&path)?;
                local::sync_directory(path.parent().unwrap())?;
                current.present = false;
                current.generation += 1;
                save(db, &current).await?;
                commit_remote_unlocked(db, &dir.id, &snapshot_path, remote).await?;
                return Ok(());
            }
            let local_moved = snapshot_path != base.relative_path;
            let remote_moved = !remote.metadata_eq(&base.remote);
            if local_moved {
                let desired_matches = remote.name == basename(&snapshot_path)
                    && remote.parents == vec![parent.clone()];
                if remote_moved && !desired_matches {
                    return Err(Error::Precondition);
                }
                let updated = if desired_matches {
                    remote
                } else {
                    if !drive.conditional_mutations() || remote.version != base.remote.version {
                        return Err(Error::Precondition);
                    }
                    drive
                        .rename(&remote, basename(&snapshot_path), &parent)
                        .await?
                };
                let _guard = scanner.gate.lock().await;
                rebase_descendants(db, &base.relative_path, &snapshot_path, false).await?;
                commit_remote_unlocked(db, &dir.id, &snapshot_path, updated).await?;
                path_ids.insert(snapshot_path.clone(), id.into());
            } else if remote_moved {
                let target = paths.get(id).ok_or(Error::Precondition)?.clone();
                if target == snapshot_path {
                    return Err(Error::Precondition);
                }
                let destination = local::safe_path(&scanner.root, &target)?;
                if destination.try_exists()? {
                    return Err(Error::Precondition);
                }
                let _guard = scanner.gate.lock().await;
                let mut latest = latest(db, &dir.id).await?;
                if latest.generation != dir.generation {
                    return Err(Error::Changed);
                }
                // The parent chain belongs to existing cloud IDs in paths.
                // Its backing directories are adopted by the next scanner
                // pass rather than creating additional cloud folders.
                durable_move_parents(&scanner.root, &target)?;
                latest.move_journal = Some(DirectoryMove {
                    source: snapshot_path,
                    target: target.clone(),
                    remote,
                });
                save(db, &latest).await?;
                recover(db, &scanner.root).await?;
                path_ids.insert(target, id.into());
            } else if remote.version != base.remote.version {
                commit_remote(db, scanner, &dir.id, &snapshot_path, remote).await?;
            }
            // A parent rename rebases a child's baseline before this iteration.
            // Publish its current local path even when the child's own metadata
            // is unchanged, so deeper empty directories resolve their parent.
            let current = latest(db, &dir.id).await?;
            path_ids.insert(current.relative_path, id.into());
            Ok(())
        }
        .await;
        if let Err(error) = result {
            if matches!(error, Error::Precondition) {
                let _guard = scanner.gate.lock().await;
                let mut current = latest(db, &observed.id).await?;
                current.review_required = true;
                save(db, &current).await?;
            }
            errors.push(format!("folder {}: {error}", observed.relative_path));
        }
    }
    Ok(errors)
}

/// Run only after file reconciliation. Never cascade Trash into live children,
/// including files created remotely since the last inventory was fetched.
pub async fn reconcile_after(
    db: &Database,
    scanner: &Scanner,
    drive: &dyn DriveClient,
    _root_id: &str,
) -> Result<Vec<String>> {
    let mut dirs = directories(db).await?;
    dirs.sort_by_key(|d| std::cmp::Reverse(d.relative_path.matches('/').count()));
    let mut errors = Vec::new();
    for dir in dirs.into_iter().filter(|d| {
        !d.present
            && !d.review_required
            && d.remote_id.is_some()
            && !d.baseline.as_ref().is_some_and(|b| b.remote.trashed)
    }) {
        let result: Result<()> = async {
            let current = drive.get(dir.remote_id.as_deref().unwrap()).await?;
            let remote = match current {
                Some(remote) => remote,
                None if dir.baseline.is_none() => return Ok(()), // reserved but never created
                None => return Err(Error::Precondition),
            };
            let metadata_matches = if let Some(base) = &dir.baseline {
                remote.metadata_eq(&base.remote)
            } else {
                dir.creation_path
                    .as_ref()
                    .is_some_and(|path| remote.name == basename(path))
                    && dir
                        .creation_parent
                        .as_ref()
                        .is_some_and(|parent| remote.parents == vec![parent.clone()])
            };
            if remote.unavailable
                || !remote.folder()
                || !metadata_matches
                || !drive.conditional_mutations()
            {
                return Err(Error::Precondition);
            }
            let latest = latest(db, &dir.id).await?;
            if latest.present
                || latest.generation != dir.generation
                || local::safe_path(&scanner.root, &latest.relative_path)?.try_exists()?
            {
                return Err(Error::Changed);
            }
            // Re-run the adapter's empty-folder postcheck even after an
            // ambiguous commit. Until this succeeds the untrashed baseline
            // remains the durable pending-delete intent in SQLite.
            let updated = drive.trash_empty_folder(&remote).await?;
            commit_remote(db, scanner, &dir.id, &dir.relative_path, updated).await
        }
        .await;
        if let Err(error) = result {
            errors.push(format!("folder {}: {error}", dir.relative_path));
        }
    }
    Ok(errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(id: &str, path: &str, inode: u64) -> Directory {
        Directory {
            id: id.into(),
            relative_path: path.into(),
            device: 1,
            inode,
            present: true,
            generation: 0,
            remote_id: None,
            baseline: None,
            creation_path: None,
            creation_parent: None,
            review_required: false,
            move_journal: None,
        }
    }

    fn assert_index_matches_linear_order(index: &ScanIndex, directories: &[Directory]) {
        for seen in [HashSet::new(), HashSet::from(["first".into()])] {
            for path in ["a", "b", "moved", "missing"] {
                for inode in [11, 22, 44, 99] {
                    let linear = directories
                        .iter()
                        .position(|directory| directory.device == 1 && directory.inode == inode)
                        .or_else(|| {
                            directories.iter().position(|directory| {
                                directory.relative_path == path && !seen.contains(&directory.id)
                            })
                        });
                    assert_eq!(
                        index.find(directories, &seen, path, 1, inode),
                        linear,
                        "path={path} inode={inode} seen={seen:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn scan_index_preserves_inode_priority_duplicate_order_and_unseen_path_fallback() {
        let mut directories = vec![
            directory("first", "a", 11),
            directory("duplicate-inode", "b", 11),
            directory("duplicate-path", "a", 22),
        ];
        let mut index = ScanIndex::new(&directories);
        assert_index_matches_linear_order(&index, &directories);

        let mut moved = directories[0].clone();
        moved.relative_path = "moved".into();
        moved.inode = 44;
        index.replace(0, &directories[0], &moved);
        directories[0] = moved;
        assert_index_matches_linear_order(&index, &directories);

        let mut recreated = directories[1].clone();
        recreated.relative_path = "a".into();
        recreated.inode = 22;
        index.replace(1, &directories[1], &recreated);
        directories[1] = recreated;
        assert_index_matches_linear_order(&index, &directories);
    }

    #[tokio::test]
    async fn empty_directory_ledger_does_not_decode_the_remote_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("state.sqlite3"), "test")
            .await
            .unwrap();
        // Valid SQLite JSON, but not a valid RemoteFile. An empty ledger must
        // not read remote file records just to decide there is no folder work.
        sqlx::query("INSERT INTO remote_observations(id,data,pending) VALUES('unused','{}',1)")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(db.remotes().await.is_err());
        let scanner = Scanner {
            root: temp.path().join("files"),
            db: db.clone(),
            gate: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        };
        let drive = crate::drive::HttpDriveClient::new(std::sync::Arc::new(
            crate::drive::TokenFile(temp.path().join("unused-token")),
        ))
        .unwrap();
        assert!(reconcile_before(&db, &scanner, &drive, "root")
            .await
            .unwrap()
            .is_empty());
    }
}
