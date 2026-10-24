//! Offline, journaled expansion from one subtree to streamed My Drive.
//! Directory rename preserves inodes; no cloud mutation or credential rewrite.
use crate::{
    db::Database,
    engine::project_paths,
    local::{self, Scanner},
    model::RemoteFile,
    service::{self, ServiceConfig},
    Error, Result,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
const KEY: &str = "scope_migration_journal";
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub old_scope: String,
    pub new_scope: String,
    pub local_prefix: String,
    pub tracked_files: usize,
    pub pending_files: usize,
    pub already_my_drive: bool,
    pub keep_offline_target: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Journal {
    plan: Plan,
    config: ServiceConfig,
    stage: PathBuf,
    old_device: u64,
    old_inode: u64,
    mode: u32,
    backup: PathBuf,
    committed: bool,
}
struct Locks {
    _engine: File,
    _mount: File,
}
fn locks(state: &Path) -> Result<Locks> {
    fn take(p: PathBuf) -> Result<File> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(p)?;
        f.try_lock_exclusive().map_err(|_| {
            Error::Invalid("Hentikan service dan unmount drive online sebelum migrasi.".into())
        })?;
        Ok(f)
    }
    Ok(Locks {
        _engine: take(state.join("engine.lock"))?,
        _mount: take(state.join("online-drive.lock"))?,
    })
}
async fn db(state: &Path, account: &str) -> Result<Database> {
    Database::read_only(&state.join("state.sqlite3"), account).await
}
pub async fn pending(state: &Path, account: &str) -> Result<bool> {
    let d = db(state, account).await?;
    let p = d.setting(KEY).await?.is_some();
    d.pool.close().await;
    Ok(p)
}
pub async fn preview(state: &Path, account: &str, root: &RemoteFile) -> Result<Plan> {
    let d = db(state, account).await?;
    if let Some(j) = d.setting(KEY).await? {
        let j: Journal = serde_json::from_str(&j)?;
        d.pool.close().await;
        return Ok(j.plan);
    }
    let result = make_plan(&d, root).await;
    d.pool.close().await;
    result
}
async fn make_plan(d: &Database, root: &RemoteFile) -> Result<Plan> {
    if !root.folder() || root.trashed || root.unavailable {
        return Err(Error::Invalid("My Drive root unavailable".into()));
    }
    let old = d
        .setting("resolved_folder")
        .await?
        .ok_or_else(|| Error::Invalid("Inventaris awal harus selesai sebelum migrasi.".into()))?;
    let files = d.files().await?;
    let has_directory_ledger: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='materialized_directories'",
    )
    .fetch_one(&d.pool)
    .await?;
    if has_directory_ledger != 0
        && crate::directory_sync::directories(d)
            .await?
            .iter()
            .any(|dir| dir.move_journal.is_some())
    {
        return Err(Error::Invalid(
            "Recover pending directory moves before expanding scope.".into(),
        ));
    }
    if !d.operations().await?.is_empty() || !d.displaced_files().await?.is_empty() {
        return Err(Error::Invalid("Selesaikan/recover operasi transfer aktif sebelum migrasi; perubahan lokal belum terunggah tetap dapat dipertahankan.".into()));
    }
    let already = old == root.id;
    if !already && d.cursor("inventory").await?.as_deref() != Some("complete") {
        return Err(Error::Invalid(
            "Selesaikan inventaris metadata sebelum memperluas scope.".into(),
        ));
    }
    let remotes = d.remotes().await?;
    let prefix = if already {
        String::new()
    } else {
        project_paths(&remotes,&root.id).get(&old).cloned().ok_or_else(||Error::Invalid("Folder lama bukan subtree My Drive pada inventaris terakhir. Sinkronkan metadata dahulu.".into()))?
    };
    if !already {
        local::safe_path(Path::new("/"), &prefix)?;
    }
    Ok(Plan {
        old_scope: old.clone(),
        new_scope: root.id.clone(),
        local_prefix: prefix,
        tracked_files: files.len(),
        pending_files: files
            .iter()
            .filter(|f| f.status != "synced" && f.status != "online_only")
            .count(),
        already_my_drive: already,
        keep_offline_target: if already {
            String::new()
        } else {
            format!("r:{old}")
        },
    })
}
fn matches(path: &Path, j: &Journal) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|m| m.is_dir() && m.dev() == j.old_device && m.ino() == j.old_inode)
}
fn prefix(p: &str, relative: &str) -> String {
    if relative.is_empty() {
        p.into()
    } else {
        format!("{p}/{relative}")
    }
}

fn durable_parents(root: &Path, destination: &Path) -> Result<()> {
    let relative = destination
        .strip_prefix(root)
        .map_err(|_| Error::Invalid("migration parent escaped root".into()))?;
    let mut parent = root.to_path_buf();
    local::sync_directory(&parent)?;
    for part in relative.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            return Err(Error::Invalid("migration parent traversal".into()));
        }
        let child = parent.join(part);
        if !child.try_exists()? {
            fs::create_dir(&child)?;
        }
        let metadata = fs::symlink_metadata(&child)?;
        if !metadata.is_dir() || metadata.is_symlink() {
            return Err(Error::Invalid("unsafe migration parent".into()));
        }
        // Each newly created link must survive before the SQLite commit marker.
        local::sync_directory(&child)?;
        local::sync_directory(&parent)?;
        parent = child;
    }
    Ok(())
}

/// Persist backup and recovery intent before any path changes. Engine/mount refuse
/// to start until resume completes; pending local content remains on its original inode.
pub async fn begin(state: &Path, account: &str, root: &RemoteFile) -> Result<Plan> {
    let _locks = locks(state)?;
    let d = Database::open(&state.join("state.sqlite3"), account).await?;
    if let Some(raw) = d.setting(KEY).await? {
        let j: Journal = serde_json::from_str(&raw)?;
        d.pool.close().await;
        return Ok(j.plan);
    }
    let config = service::load_config(state)?;
    if config.account != account {
        return Err(Error::Invalid("profile account mismatch".into()));
    }
    if d.setting("root").await?.as_deref() != config.root.to_str()
        || d.setting("folder").await?.as_deref() != Some(config.drive_folder.as_str())
    {
        return Err(Error::Invalid("Konfigurasi service tidak cocok dengan binding database; migrasi dibatalkan sebelum memindai file.".into()));
    }
    let scanner = Scanner {
        root: config.root.clone(),
        db: d.clone(),
        gate: Arc::new(Mutex::new(())),
    };
    scanner.scan().await?;
    let plan = make_plan(&d, root).await?;
    if plan.already_my_drive {
        d.pool.close().await;
        return Ok(plan);
    }
    let meta = fs::symlink_metadata(&config.root)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(Error::Invalid("backing root ownership/type".into()));
    }
    let id = uuid::Uuid::new_v4();
    let stage = config
        .root
        .parent()
        .ok_or_else(|| Error::Invalid("root parent".into()))?
        .join(format!(".mygdrive-scope-{id}"));
    local::private_dir(&state.join("backups"))?;
    let backup = state
        .join("backups")
        .join(format!("before-my-drive-{id}.sqlite3"));
    sqlx::query("VACUUM INTO ?")
        .bind(
            backup
                .to_str()
                .ok_or_else(|| Error::Invalid("backup path".into()))?,
        )
        .execute(&d.pool)
        .await?;
    fs::set_permissions(&backup, fs::Permissions::from_mode(0o600))?;
    local::sync_directory(backup.parent().unwrap())?;
    let j = Journal {
        plan: plan.clone(),
        config,
        stage,
        old_device: meta.dev(),
        old_inode: meta.ino(),
        mode: meta.mode() & 0o777,
        backup,
        committed: false,
    };
    sqlx::query("INSERT INTO settings(key,value) VALUES(?,?)")
        .bind(KEY)
        .bind(serde_json::to_string(&j)?)
        .execute(&d.pool)
        .await?;
    d.pool.close().await;
    Ok(plan)
}

/// Idempotent across a crash after either rename or the database commit.
pub async fn resume(state: &Path, account: &str) -> Result<Plan> {
    let _locks = locks(state)?;
    let d = Database::open(&state.join("state.sqlite3"), account).await?;
    let raw = d
        .setting(KEY)
        .await?
        .ok_or_else(|| Error::Invalid("No migration to recover".into()))?;
    let mut j: Journal = serde_json::from_str(&raw)?;
    let root = j.config.root.clone();
    if !j.committed {
        let target = local::safe_path(&root, &j.plan.local_prefix)?;
        if matches(&root, &j) {
            if j.stage.try_exists()? {
                return Err(Error::Invalid(
                    "Migration staging collision; preserve both directories for review.".into(),
                ));
            }
            fs::rename(&root, &j.stage)?;
            local::sync_directory(root.parent().unwrap())?;
        }
        if let Ok(m) = fs::symlink_metadata(&root) {
            if !m.is_dir() || m.is_symlink() || m.uid() != unsafe { libc::geteuid() } {
                return Err(Error::Invalid(
                    "Migration root replaced; refusing unsafe recovery".into(),
                ));
            }
        }
        if !root.try_exists()? {
            fs::create_dir(&root)?;
            fs::set_permissions(&root, fs::Permissions::from_mode(j.mode))?;
            local::sync_directory(root.parent().unwrap())?;
        }
        if !matches(&target, &j) {
            if !matches(&j.stage, &j) || target.try_exists()? {
                return Err(Error::Invalid("Migration paths changed; no files overwritten. Recovery requires original staging directory.".into()));
            }
            durable_parents(&root, target.parent().unwrap())?;
            fs::rename(&j.stage, &target)?;
        }
        // Recovery may observe the target inode after a crash immediately following
        // rename. Re-establish durability even when that rename need not be replayed.
        durable_parents(&root, target.parent().unwrap())?;
        if !matches(&target, &j) {
            return Err(Error::Changed);
        }
        local::sync_directory(target.parent().unwrap())?;
        local::sync_directory(root.parent().unwrap())?;
        let mut files = d.files().await?;
        let mut directories = crate::directory_sync::directories(&d).await?;
        let rules: Vec<(String, bool)> =
            sqlx::query_as("SELECT target,pinned FROM offline_selections")
                .fetch_all(&d.pool)
                .await?;
        let cached: Vec<(String, String)> =
            sqlx::query_as("SELECT key,value FROM settings WHERE key LIKE 'folder_id:%'")
                .fetch_all(&d.pool)
                .await?;
        let remotes = d.remotes().await?;
        let projected = project_paths(&remotes, &j.plan.new_scope);
        let root_meta = fs::metadata(&root)?;
        let mut tx = d.pool.begin().await?;
        // Avoid transient uniqueness collisions (a.txt -> Test/a.txt while Test/a.txt exists).
        for f in &files {
            sqlx::query("UPDATE files SET relative_path=? WHERE id=?")
                .bind(format!("{}migrate/{}", local::INTERNAL_PREFIX, f.id))
                .bind(&f.id)
                .execute(&mut *tx)
                .await?;
        }
        for f in &mut files {
            f.relative_path = prefix(&j.plan.local_prefix, &f.relative_path);
            f.generation += 1;
            if let Some(base) = &mut f.baseline {
                base.relative_path = prefix(&j.plan.local_prefix, &base.relative_path);
                if let Some(p) = &mut base.remote_path {
                    *p = prefix(&j.plan.local_prefix, p);
                }
            }
            sqlx::query("UPDATE files SET relative_path=?,generation=?,data=? WHERE id=?")
                .bind(&f.relative_path)
                .bind(f.generation)
                .bind(serde_json::to_string(f)?)
                .bind(&f.id)
                .execute(&mut *tx)
                .await?;
        }
        for directory in &mut directories {
            directory.relative_path = prefix(&j.plan.local_prefix, &directory.relative_path);
            directory.generation += 1;
            if let Some(path) = &mut directory.creation_path {
                *path = prefix(&j.plan.local_prefix, path);
            }
            if let Some(base) = &mut directory.baseline {
                base.relative_path = prefix(&j.plan.local_prefix, &base.relative_path);
            }
            if let Some(journal) = &mut directory.move_journal {
                journal.source = prefix(&j.plan.local_prefix, &journal.source);
                journal.target = prefix(&j.plan.local_prefix, &journal.target);
            }
            sqlx::query("UPDATE materialized_directories SET relative_path=?,data=? WHERE id=?")
                .bind(&directory.relative_path)
                .bind(serde_json::to_string(directory)?)
                .bind(&directory.id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM offline_selections WHERE target='root' OR target LIKE 'p:%'")
            .execute(&mut *tx)
            .await?;
        for (target, pinned) in rules {
            if let Some(path) = target.strip_prefix("p:") {
                let new = format!("p:{}", prefix(&j.plan.local_prefix, path));
                sqlx::query("INSERT OR IGNORE INTO offline_selections(target,pinned) VALUES(?,?)")
                    .bind(new)
                    .bind(pinned)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        // Preserve the old subtree offline; never pin the expanded My Drive root.
        sqlx::query("INSERT INTO offline_selections(target,pinned) VALUES(?,1) ON CONFLICT(target) DO UPDATE SET pinned=1").bind(&j.plan.keep_offline_target).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM settings WHERE key LIKE 'folder_id:%'")
            .execute(&mut *tx)
            .await?;
        for (key, value) in cached {
            let new = format!(
                "folder_id:{}",
                prefix(
                    &j.plan.local_prefix,
                    key.strip_prefix("folder_id:").unwrap()
                )
            );
            sqlx::query("INSERT INTO settings(key,value) VALUES(?,?)")
                .bind(new)
                .bind(value)
                .execute(&mut *tx)
                .await?;
        }
        // Anchor every ancestor by ID, including Drive duplicate-name aliases.
        for r in remotes.iter().filter(|r| r.folder()) {
            if let Some(path) = projected.get(&r.id).filter(|p| {
                **p == j.plan.local_prefix || j.plan.local_prefix.starts_with(&format!("{p}/"))
            }) {
                sqlx::query("INSERT OR REPLACE INTO settings(key,value) VALUES(?,?)")
                    .bind(format!("folder_id:{path}"))
                    .bind(&r.id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        for (key, value) in [
            ("folder", "root".into()),
            ("resolved_folder", j.plan.new_scope.clone()),
            ("storage_mode", "online_only".into()),
            ("namespace_mode", "stream_my_drive".into()),
            (
                "root_identity",
                format!("{}:{}", root_meta.dev(), root_meta.ino()),
            ),
        ] {
            sqlx::query("INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value").bind(key).bind(value).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE conflicts SET relative_path=? || '/' || relative_path")
            .bind(&j.plan.local_prefix)
            .execute(&mut *tx)
            .await?;
        j.committed = true;
        sqlx::query("UPDATE settings SET value=? WHERE key=?")
            .bind(serde_json::to_string(&j)?)
            .bind(KEY)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    let mut config = j.config.clone();
    config.drive_folder = "root".into();
    service::atomic_json(&service::config_path(state), &config)?;
    service::atomic_json(&state.join("last-scope-migration.json"), &j)?;
    sqlx::query("DELETE FROM settings WHERE key=?")
        .bind(KEY)
        .execute(&d.pool)
        .await?;
    d.pool.close().await;
    Ok(j.plan)
}
