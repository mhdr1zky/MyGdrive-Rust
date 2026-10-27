//! Read-only disk/problem inventory and single-writer maintenance commands.
use crate::{db::Database, local, model::LocalFile, service, Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    ffi::CString,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
    path::{Path, PathBuf},
};

pub const PAGE_SIZE: usize = 25;
const ISSUES: &str = "(coalesce(json_extract(f.data,'$.review_required'),0)=1 OR f.sync_status LIKE '%review%' OR json_extract(f.data,'$.metadata_issue') IS NOT NULL OR EXISTS (SELECT 1 FROM sync_operations o WHERE o.file_id=f.id AND o.stage NOT IN ('done','cancelled') AND json_extract(o.data,'$.last_error') IS NOT NULL))";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    ApproveRecovery {
        file_id: String,
        generation: i64,
        sha256: String,
    },
    PruneCache {
        max_bytes: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    RecoveryApproved,
    CachePruned {
        removed_objects: usize,
        reclaimed_bytes: u64,
        protected_bytes: u64,
    },
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub file: LocalFile,
    pub diagnostic: Option<crate::diagnostics::FileIssue>,
}

#[derive(Debug, Clone)]
pub struct Page {
    pub root: PathBuf,
    pub issues: Vec<Issue>,
    pub total_issues: usize,
    pub next: Option<usize>,
    pub live: bool,
    pub paused: bool,
    pub storage: Option<Storage>,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    /// Allocated blocks, rather than apparent size, for sparse files.
    pub bytes: u64,
    pub files: u64,
}
#[derive(Debug, Clone, Default)]
pub struct Storage {
    pub backing: Usage,
    pub cache: Usage,
    pub outbox: Usage,
    pub recovery: Usage,
    pub other: Usage,
    pub free_backing_bytes: Option<u64>,
    pub free_state_bytes: Option<u64>,
    /// An estimate taken during sync: skipped symlinks/errors make it incomplete.
    pub skipped: u64,
}

fn free_bytes(path: &Path) -> Option<u64> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // statvfs writes the complete struct on success; CString lives through the call.
    if unsafe { libc::statvfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
        return None;
    }
    let info = unsafe { info.assume_init() };
    Some(info.f_bavail.saturating_mul(info.f_frsize))
}

/// Never reads file contents or follows symlinks, including a replaced scan root.
/// Count hardlinked inodes only once across all categories.
pub fn measure(root: &Path, state: &Path) -> Storage {
    let mut storage = Storage {
        free_backing_bytes: free_bytes(root),
        free_state_bytes: free_bytes(state),
        ..Default::default()
    };
    let mut seen = HashSet::new();
    for (base, is_state) in [(root, false), (state, true)] {
        for item in walkdir::WalkDir::new(base)
            .follow_links(false)
            .follow_root_links(false)
        {
            let Ok(item) = item else {
                storage.skipped += 1;
                continue;
            };
            let Ok(meta) = std::fs::symlink_metadata(item.path()) else {
                storage.skipped += 1;
                continue;
            };
            if meta.is_dir() {
                continue;
            }
            if !meta.is_file() {
                // The service socket has no file payload to account for. Its
                // presence must not mark every live profile as incomplete.
                if is_state
                    && meta.file_type().is_socket()
                    && item.path() == service::socket_path(state)
                {
                    continue;
                }
                storage.skipped += 1;
                continue;
            }
            if !seen.insert((meta.dev(), meta.ino())) {
                continue;
            }
            let relative = item
                .path()
                .strip_prefix(base)
                .expect("walked path inside base");
            let name = item.file_name().to_string_lossy();
            let usage = if !is_state {
                if name.starts_with(local::INTERNAL_PREFIX) {
                    if name.ends_with(".swap") || name.ends_with(".removed") {
                        &mut storage.recovery
                    } else {
                        &mut storage.other
                    }
                } else {
                    &mut storage.backing
                }
            } else {
                match relative
                    .components()
                    .next()
                    .and_then(|c| c.as_os_str().to_str())
                {
                    Some("cache") => &mut storage.cache,
                    Some("outbox") => &mut storage.outbox,
                    Some("recovery") => &mut storage.recovery,
                    _ => &mut storage.other,
                }
            };
            usage.bytes = usage
                .bytes
                .saturating_add(meta.blocks().saturating_mul(512));
            usage.files += 1;
        }
    }
    storage
}

/// Bounded, consistent WAL read. Expensive directory enumeration is opt-in.
pub async fn browse(state: &Path, offset: usize, include_storage: bool) -> Result<Page> {
    let config = service::load_config(state)?;
    let db = Database::read_only(&state.join("state.sqlite3"), &config.account).await?;
    let result = async {
        let mut tx = db.pool.begin().await?;
        let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM files f WHERE {ISSUES}"))
            .fetch_one(&mut *tx).await?;
        let rows: Vec<String> = sqlx::query_scalar(&format!("SELECT f.data FROM files f WHERE {ISSUES} ORDER BY f.relative_path,f.id LIMIT ? OFFSET ?"))
            .bind((PAGE_SIZE + 1) as i64).bind(i64::try_from(offset).map_err(|_| Error::Invalid("invalid page".into()))?)
            .fetch_all(&mut *tx).await?;
        let next = (rows.len() > PAGE_SIZE).then_some(offset.saturating_add(PAGE_SIZE));
        let mut issues = Vec::new();
        for row in rows.into_iter().take(PAGE_SIZE) {
            let file: LocalFile = serde_json::from_str(&row)?;
            let error: Option<String> = sqlx::query_scalar("SELECT json_extract(data,'$.last_error') FROM sync_operations WHERE file_id=? AND stage NOT IN ('done','cancelled') AND json_extract(data,'$.last_error') IS NOT NULL ORDER BY rowid DESC LIMIT 1")
                .bind(&file.id).fetch_optional(&mut *tx).await?;
            let diagnostic = file.metadata_issue.or_else(|| error.as_deref().map(crate::diagnostics::FileIssue::from_legacy));
            issues.push(Issue { file, diagnostic });
        }
        tx.commit().await?;
        let status = service::send(state, &service::Request::new(service::Action::Status)).await.ok()
            .filter(|reply| reply.accepted && !reply.status.shutdown_requested);
        let live = status.is_some();
        let paused = status.as_ref().is_none_or(|reply| reply.status.paused);
        let storage = if include_storage {
            let root = config.root.clone(); let state = state.to_path_buf();
            Some(tokio::task::spawn_blocking(move || measure(&root, &state)).await.map_err(|_| Error::Invalid("storage worker stopped".into()))?)
        } else { None };
        Ok(Page { root: config.root, issues, total_issues: total.max(0) as usize, next, live, paused, storage })
    }.await;
    db.pool.close().await;
    result
}

pub async fn apply(state: &Path, command: Request) -> Result<Outcome> {
    let status = service::send(state, &service::Request::new(service::Action::Status)).await?;
    if !status.accepted {
        return Err(Error::Changed);
    }
    let mut request = service::Request::new(service::Action::Maintenance);
    request.session_id = Some(status.status.session_id);
    request.maintenance = Some(command);
    // Reuse the same durable request ID if the ACK is lost.
    let reply = match service::send(state, &request).await {
        Ok(reply) => reply,
        Err(_) => service::send(state, &request).await?,
    };
    if !reply.accepted {
        return Err(Error::Changed);
    }
    reply
        .maintenance
        .ok_or_else(|| Error::Invalid("restart service to enable maintenance".into()))
}

/// Validated local folder URI; contents are never opened as an implicit download.
pub fn folder_uri(root: &Path, relative: &str) -> Result<String> {
    let metadata = std::fs::symlink_metadata(root)?;
    if !metadata.is_dir() {
        return Err(Error::Changed);
    }
    let path = local::safe_path(root, relative)?;
    let parent = path.parent().ok_or(Error::Changed)?;
    reqwest::Url::from_directory_path(parent)
        .map(|uri| uri.to_string())
        .map_err(|_| Error::Invalid("invalid local folder".into()))
}

pub fn drive_uri(remote_id: &str) -> Option<String> {
    (!remote_id.is_empty()
        && remote_id.len() <= 256
        && remote_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    .then(|| format!("https://drive.google.com/file/d/{remote_id}/view"))
}
