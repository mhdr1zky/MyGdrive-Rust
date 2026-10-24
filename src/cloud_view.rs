//! Metadata namespace shared by the virtual drive and Thunar actions.
use crate::{db::Database, local, model::RemoteFile, offline::SCOPED_METADATA_SQL, Error, Result};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

// The namespace never needs baselines, hashes or generation history. Keep
// those in the state DB rather than decoding them for every visible file.
#[derive(sqlx::FromRow)]
struct NamespaceFile {
    id: String,
    relative_path: String,
    remote_id: Option<String>,
    status: String,
    materialized: bool,
    size: Option<i64>,
    review_required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub key: String,
    pub folder: bool,
    pub size: u64,
    pub file_id: Option<String>,
    pub unsupported: bool,
}
#[derive(Clone)]
pub struct View {
    pub root: PathBuf,
    pub entries: Arc<BTreeMap<String, Entry>>,
    pub folder_badges: HashMap<String, crate::availability::FolderBadge>,
}
impl View {
    pub async fn load(db: &Database) -> Result<Self> {
        if db.setting("scope_migration_journal").await?.is_some() {
            return Err(Error::Invalid(
                "Migrasi scope sedang berlangsung; tunggu recovery selesai.".into(),
            ));
        }
        let root = PathBuf::from(
            db.setting("root")
                .await?
                .ok_or_else(|| Error::Invalid("missing root".into()))?,
        );
        let scope = db
            .setting("resolved_folder")
            .await?
            .or(db.setting("folder").await?)
            .ok_or_else(|| Error::Invalid("missing scope".into()))?;
        let all_my_drive = db.setting("folder").await?.as_deref() == Some("root");
        let remotes: Vec<RemoteFile> = if all_my_drive {
            db.remotes().await?
        } else {
            sqlx::query_scalar::<_, String>(SCOPED_METADATA_SQL)
                .bind(&scope)
                .fetch_all(&db.pool)
                .await?
                .into_iter()
                .map(|data| Ok(serde_json::from_str(&data)?))
                .collect::<Result<_>>()?
        };
        let paths = crate::engine::project_paths(&remotes, &scope);
        let files: Vec<NamespaceFile> = sqlx::query_as("SELECT id,relative_path,remote_id,sync_status AS status,coalesce(json_extract(data,'$.materialized'),0) AS materialized,json_extract(data,'$.fingerprint.digest.size') AS size,coalesce(json_extract(data,'$.review_required'),0) AS review_required FROM files")
            .fetch_all(&db.pool).await?;
        let by_remote: HashMap<_, _> = files
            .iter()
            .filter_map(|f| f.remote_id.as_deref().map(|id| (id, f)))
            .collect();
        let by_path: HashMap<_, _> = files
            .iter()
            .map(|f| (f.relative_path.as_str(), f))
            .collect();
        let mut entries = BTreeMap::new();
        let mut present = HashSet::new();
        entries.insert(
            String::new(),
            Entry {
                path: String::new(),
                key: "root".into(),
                folder: true,
                size: 0,
                file_id: None,
                unsupported: false,
            },
        );
        for remote in remotes {
            let Some(projected) = paths.get(&remote.id) else {
                continue;
            };
            let file = by_remote.get(remote.id.as_str()).copied();
            if file.is_some_and(|f| f.status == "tombstone") {
                continue;
            }
            let path = file
                .map(|f| f.relative_path.clone())
                .unwrap_or_else(|| projected.clone());
            // A disappeared materialized path is a pending local move/delete, not a new placeholder.
            if file.is_some_and(|f| f.materialized)
                && !local::safe_path(&root, &path)?.try_exists()?
            {
                continue;
            }
            let size = file
                .and_then(|f| f.size)
                .map(|size| size.max(0) as u64)
                .unwrap_or_else(|| {
                    remote
                        .size
                        .as_deref()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0)
                });
            entries.insert(
                path.clone(),
                Entry {
                    path,
                    key: format!("r:{}", remote.id),
                    folder: remote.folder(),
                    size,
                    file_id: file.map(|f| f.id.clone()),
                    unsupported: remote.workspace()
                        || remote.capabilities.can_download == Some(false),
                },
            );
        }
        // Include unsynchronized local files/directories immediately, without content hashing.
        for item in walkdir::WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                !e.file_name()
                    .to_string_lossy()
                    .starts_with(local::INTERNAL_PREFIX)
            })
        {
            let item =
                item.map_err(|e| Error::Invalid(format!("local namespace unavailable: {e}")))?;
            if item.path() == root || item.file_type().is_symlink() {
                continue;
            }
            if !item.file_type().is_dir() && !item.file_type().is_file() {
                continue;
            }
            let path = item
                .path()
                .strip_prefix(&root)
                .unwrap()
                .to_str()
                .ok_or_else(|| Error::Unsupported("non-UTF8 filename".into()))?
                .to_string();
            let file = by_path.get(path.as_str()).copied();
            if item.file_type().is_file() {
                present.insert(path.clone());
            }
            if let Some(e) = entries.get_mut(&path) {
                e.size = item
                    .metadata()
                    .map_err(|e| Error::Invalid(e.to_string()))?
                    .len();
                continue;
            }
            entries.insert(
                path.clone(),
                Entry {
                    key: if item.file_type().is_dir() {
                        format!("p:{path}")
                    } else {
                        file.map(|f| format!("l:{}", f.id))
                            .unwrap_or_else(|| format!("p:{path}"))
                    },
                    path,
                    folder: item.file_type().is_dir(),
                    size: item
                        .metadata()
                        .map_err(|e| Error::Invalid(e.to_string()))?
                        .len(),
                    file_id: file.map(|f| f.id.clone()),
                    unsupported: false,
                },
            );
        }
        let names: Vec<_> = entries.keys().cloned().collect();
        for name in names {
            let mut parent = Path::new(&name).parent();
            while let Some(p) = parent {
                let path = p.to_str().unwrap().to_string();
                entries.entry(path.clone()).or_insert_with(|| Entry {
                    key: format!("p:{path}"),
                    path,
                    folder: true,
                    size: 0,
                    file_id: None,
                    unsupported: false,
                });
                parent = p.parent();
            }
        }
        let rules =
            sqlx::query_as::<_, (String, bool)>("SELECT target,pinned FROM offline_selections")
                .fetch_all(&db.pool)
                .await?
                .into_iter()
                .collect();
        let complete = db.cursor("inventory").await?.as_deref() == Some("complete");
        let ready = files
            .iter()
            .filter(|f| {
                f.materialized
                    && f.size.is_some()
                    && !f.review_required
                    && present.contains(&f.relative_path)
            })
            .map(|f| f.relative_path.clone())
            .collect();
        let folder_badges =
            crate::availability::folder_badges_from_ready(&entries, &rules, &ready, complete);
        Ok(Self {
            root,
            entries: Arc::new(entries),
            folder_badges,
        })
    }
    pub fn target(&self, relative: &str) -> Result<&Entry> {
        self.entries.get(relative).ok_or_else(|| {
            Error::Invalid(
                "Pilihan tidak ditemukan pada drive MyGdrive Rust. Muat ulang folder.".into(),
            )
        })
    }
}

/// Validate lexically before resolving symlinks; never follow a selected symlink outside a drive.
pub fn relative_selection(base: &Path, selected: &Path) -> Result<String> {
    let rel = selected
        .strip_prefix(base)
        .map_err(|_| Error::Invalid("Pilihan berada di luar drive MyGdrive Rust.".into()))?;
    if rel.as_os_str().is_empty() {
        return Ok(String::new());
    }
    let rel = rel
        .to_str()
        .ok_or_else(|| Error::Unsupported("non-UTF8 path".into()))?;
    local::safe_path(base, rel)?;
    Ok(rel.into())
}

/// Do not open an empty, unmounted directory if startup failed.
pub fn mounted_online_root(state: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(state.join("virtual-drive.json")).ok()?;
    if bytes.len() > 16384 {
        return None;
    }
    let config: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let mount = PathBuf::from(config.get("mount")?.as_str()?);
    if !mount.is_absolute() {
        return None;
    }
    let escaped = mount
        .to_str()?
        .replace('\\', "\\134")
        .replace(' ', "\\040")
        .replace('\t', "\\011")
        .replace('\n', "\\012");
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    mounts
        .lines()
        .any(|line| {
            let Some((fields, extra)) = line.split_once(" - ") else {
                return false;
            };
            fields.split_whitespace().nth(4) == Some(escaped.as_str())
                && extra.split_whitespace().nth(1) == Some("mygdrive")
        })
        .then_some(mount)
}
