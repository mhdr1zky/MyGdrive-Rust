//! Durable, additive offline selections. Unpin never deletes local or remote data.
use crate::{
    db::Database,
    local,
    model::{LocalFile, RemoteFile},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    os::unix::fs::MetadataExt,
    path::Path,
};

// Unary + removes the CTE id's TEXT affinity so SQLite can search the JSON
// expression index at every recursive step instead of scanning the account.
pub const SCOPED_METADATA_SQL: &str = "WITH RECURSIVE tree(id,data) AS (SELECT id,data FROM remote_observations WHERE json_extract(data,'$.parents[0]')=? AND coalesce(json_extract(data,'$.trashed'),0)=0 AND coalesce(json_extract(data,'$.unavailable'),0)=0 UNION SELECT r.id,r.data FROM tree t JOIN remote_observations r ON json_extract(r.data,'$.parents[0]')=+t.id WHERE coalesce(json_extract(r.data,'$.trashed'),0)=0 AND coalesce(json_extract(r.data,'$.unavailable'),0)=0) SELECT data FROM tree";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    List {
        parent: String,
        offset: usize,
    },
    Set {
        target: String,
        pinned: bool,
    },
    Hydrate {
        file_id: String,
    },
    Metadata {
        file_id: String,
        source: String,
        destination: Option<String>,
    },
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Counts {
    pub total: u64,
    pub ready: u64,
    pub unsupported: u64,
    pub dirty: u64,
    pub review: u64,
}
impl Counts {
    fn add(&mut self, other: &Self) {
        self.total += other.total;
        self.ready += other.ready;
        self.unsupported += other.unsupported;
        self.dirty += other.dirty;
        self.review += other.review;
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub key: String,
    pub name: String,
    pub folder: bool,
    pub selected: bool,
    pub inherited: bool,
    pub effective: bool,
    pub counts: Counts,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub current: Node,
    pub parent: Option<String>,
    pub children: Vec<Node>,
    pub next: Option<usize>,
    pub inventory_complete: bool,
    pub last_verified_ms: Option<i64>,
    pub local_root: std::path::PathBuf,
}
struct Entry {
    node: Node,
    parent: Option<String>,
    children: Vec<String>,
    file: Option<LocalFile>,
}
pub struct Graph {
    entries: HashMap<String, Entry>,
    rules: HashMap<String, bool>,
    inventory_complete: bool,
    last_verified_ms: Option<i64>,
    local_root: std::path::PathBuf,
    aliases: Vec<(String, String)>,
}
fn node(key: String, name: String, folder: bool, parent: Option<String>) -> Entry {
    Entry {
        node: Node {
            key,
            name: name.chars().take(512).collect(),
            folder,
            selected: false,
            inherited: false,
            effective: false,
            counts: Counts::default(),
        },
        parent,
        children: vec![],
        file: None,
    }
}
impl Graph {
    pub async fn load(db: &Database, verify_local: bool) -> Result<Self> {
        // All metadata comes from a single SQLite snapshot, never a network request.
        let mut tx = db.pool.begin().await?;
        let root: String = sqlx::query_scalar("SELECT value FROM settings WHERE key='root'")
            .fetch_one(&mut *tx)
            .await?;
        let scope: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key='resolved_folder'")
                .fetch_optional(&mut *tx)
                .await?;
        let folder: String = sqlx::query_scalar("SELECT value FROM settings WHERE key='folder'")
            .fetch_one(&mut *tx)
            .await?;
        let scope = scope.as_deref().unwrap_or(&folder);
        // Scope the browser at SQL level. Do not parse the whole account every five
        // seconds when a profile only contains a small subtree. Match the engine's
        // first-parent projection and omit inaccessible/trashed subtrees.
        // My Drive covers almost the whole account. A linear read plus the
        // existing in-memory ancestry filter avoids UNION dedup of large JSON
        // rows. Small subtree profiles retain the indexed recursive query.
        let remotes: Vec<RemoteFile> = if folder == "root" {
            crate::db::read_json_rows(&mut tx, crate::db::JsonTable::Remotes).await?
        } else {
            sqlx::query_scalar::<_, String>(SCOPED_METADATA_SQL)
                .bind(scope)
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .map(|data| Ok(serde_json::from_str(&data)?))
                .collect::<Result<_>>()?
        };
        let files: Vec<LocalFile> =
            crate::db::read_json_rows(&mut tx, crate::db::JsonTable::Files).await?;
        let rules: HashMap<String, bool> =
            sqlx::query_as::<_, (String, bool)>("SELECT target,pinned FROM offline_selections")
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .collect();
        let inventory: Option<String> =
            sqlx::query_scalar("SELECT token FROM sync_cursors WHERE name='inventory'")
                .fetch_optional(&mut *tx)
                .await?;
        let verified: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key='offline_verified_ms'")
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        let paths = crate::engine::project_paths(&remotes, scope);
        let mut entries = HashMap::new();
        let mut aliases = Vec::new();
        entries.insert(
            "root".into(),
            node(
                "root".into(),
                if folder == "root" {
                    "My Drive"
                } else {
                    "Folder sinkronisasi"
                }
                .into(),
                true,
                None,
            ),
        );
        let mut directories: HashMap<String, String> =
            HashMap::from([(String::new(), "root".into())]);
        for remote in &remotes {
            if !paths.contains_key(&remote.id) {
                continue;
            }
            let key = format!("r:{}", remote.id);
            let parent = remote.parents.first().map(|p| {
                if p == scope {
                    "root".into()
                } else {
                    format!("r:{p}")
                }
            });
            if remote.folder() {
                directories.insert(paths[&remote.id].clone(), key.clone());
            }
            let mut entry = node(key.clone(), remote.name.clone(), remote.folder(), parent);
            if remote.folder() {
                let old = format!("p:{}", paths[&remote.id]);
                if let Some(selected) = rules.get(&old) {
                    entry.node.selected = *selected;
                    aliases.push((old, key.clone()));
                }
            }
            if !remote.folder() {
                entry.node.counts.total = 1;
                if remote.workspace() || remote.capabilities.can_download == Some(false) {
                    entry.node.counts.unsupported = 1;
                }
            }
            entries.insert(key, entry);
        }
        for file in files {
            if file.status == "tombstone" && file.fingerprint.is_none() {
                continue;
            }
            let remote_key = file.remote_id.as_ref().map(|id| format!("r:{id}"));
            let key = remote_key
                .filter(|k| entries.contains_key(k))
                .unwrap_or_else(|| format!("l:{}", file.id));
            if file.remote_id.is_some()
                && !entries.contains_key(&key)
                && !file.materialized
                && file.fingerprint.is_none()
            {
                continue;
            }
            if !entries.contains_key(&key) {
                let parent_path = Path::new(&file.relative_path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("");
                let parent = ensure_directory(parent_path, &mut entries, &mut directories);
                let name = Path::new(&file.relative_path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let mut e = node(key.clone(), name, false, Some(parent));
                e.node.counts.total = 1;
                entries.insert(key.clone(), e);
            }
            let e = entries.get_mut(&key).unwrap();
            let old = format!("l:{}", file.id);
            e.node.selected = rules.get(&old).copied().unwrap_or(file.available_offline);
            if key != old && rules.contains_key(&old) {
                aliases.push((old, key.clone()));
            }
            e.node.counts.dirty = u64::from(match (&file.fingerprint, &file.baseline) {
                (Some(fp), Some(base)) => {
                    fp.digest != base.digest || file.relative_path != base.relative_path
                }
                (Some(_), None) => true,
                (None, _) => file.materialized,
            });
            e.node.counts.review = u64::from(
                file.review_required
                    || file.status.ends_with("_review")
                    || matches!(
                        file.status.as_str(),
                        "remote_unavailable" | "out_of_scope" | "metadata_pending_validation"
                    ),
            );
            if verify_local && e.node.counts.unsupported == 0 && present(Path::new(&root), &file) {
                e.node.counts.ready = 1;
            }
            e.file = Some(file);
        }
        let keys: Vec<String> = entries.keys().cloned().collect();
        for key in keys {
            if let Some(parent) = entries[&key].parent.clone() {
                if let Some(e) = entries.get_mut(&parent) {
                    e.children.push(key);
                }
            }
        }
        // Traverse once, propagate selections downward, then aggregate counts upward.
        let mut order = vec!["root".to_string()];
        let mut i = 0;
        while i < order.len() {
            let key = order[i].clone();
            i += 1;
            let inherited = entries[&key]
                .parent
                .as_ref()
                .is_some_and(|p| entries.get(p).is_some_and(|e| e.node.effective));
            let e = entries.get_mut(&key).unwrap();
            e.node.selected = rules.get(&key).copied().unwrap_or(e.node.selected);
            e.node.inherited = inherited;
            e.node.effective = e.node.selected || inherited;
            order.extend(e.children.clone());
        }
        for key in order.into_iter().rev() {
            if let Some(parent) = entries[&key].parent.clone() {
                let counts = entries[&key].node.counts.clone();
                entries.get_mut(&parent).unwrap().node.counts.add(&counts);
            }
        }
        Ok(Self {
            entries,
            rules,
            inventory_complete: inventory.as_deref() == Some("complete"),
            last_verified_ms: verified.and_then(|s| s.parse().ok()),
            local_root: root.into(),
            aliases,
        })
    }
    pub fn target_for_file(&self, id: &str) -> Option<String> {
        self.entries
            .values()
            .find(|e| e.file.as_ref().is_some_and(|f| f.id == id))
            .map(|e| e.node.key.clone())
    }
    pub fn selection(&self, key: &str) -> Option<&Node> {
        self.entries.get(key).map(|e| &e.node)
    }
    pub fn protected_remote_ids(&self) -> HashSet<String> {
        self.entries
            .values()
            .filter(|e| e.node.effective)
            .filter_map(|e| e.file.as_ref().and_then(|f| f.remote_id.clone()))
            .collect()
    }
    pub fn requested_file_ids(&self) -> HashSet<String> {
        self.entries
            .values()
            .filter(|e| e.node.effective)
            .filter_map(|e| e.file.as_ref().map(|f| f.id.clone()))
            .collect()
    }
    pub async fn promote(&self, db: &Database) -> Result<()> {
        if self.aliases.is_empty() {
            return Ok(());
        }
        let mut tx = db.pool.begin().await?;
        for (old, new) in &self.aliases {
            sqlx::query("INSERT OR IGNORE INTO offline_selections(target,pinned) SELECT ?,pinned FROM offline_selections WHERE target=?").bind(new).bind(old).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM offline_selections WHERE target=?")
                .bind(old)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub fn page(&self, parent: &str, offset: usize) -> Result<Page> {
        let e = self
            .entries
            .get(parent)
            .filter(|e| e.node.folder)
            .ok_or_else(|| {
                Error::Invalid("Folder tidak ditemukan pada inventaris profil ini.".into())
            })?;
        let mut children: Vec<_> = e
            .children
            .iter()
            .map(|k| self.entries[k].node.clone())
            .collect();
        children.sort_by(|a, b| {
            b.folder
                .cmp(&a.folder)
                .then(a.name.cmp(&b.name))
                .then(a.key.cmp(&b.key))
        });
        if offset > children.len() {
            return Err(Error::Invalid("Halaman berubah. Muat ulang folder.".into()));
        }
        let total = children.len();
        let mut page = Page {
            current: e.node.clone(),
            parent: e.parent.clone(),
            children: vec![],
            next: None,
            inventory_complete: self.inventory_complete,
            last_verified_ms: self.last_verified_ms,
            local_root: self.local_root.clone(),
        };
        for child in children.into_iter().skip(offset).take(32) {
            page.children.push(child);
            if serde_json::to_vec(&page)?.len() > 64 * 1024 {
                page.children.pop();
                break;
            }
        }
        let next = offset + page.children.len();
        if next < total {
            page.next = Some(next);
        }
        Ok(page)
    }
    pub async fn set(
        &self,
        db: &Database,
        request_id: &str,
        target: &str,
        pinned: bool,
    ) -> Result<bool> {
        if uuid::Uuid::parse_str(request_id).is_err() {
            return Err(Error::Invalid("invalid selection request ID".into()));
        }
        let payload = serde_json::to_string(&(target, pinned))?;
        let mut tx = db.pool.begin().await?;
        if let Some(old) = sqlx::query_scalar::<_, String>(
            "SELECT payload FROM offline_commands WHERE request_id=?",
        )
        .bind(request_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            if old != payload {
                return Err(Error::Invalid(
                    "Request ID sudah dipakai untuk pilihan berbeda.".into(),
                ));
            }
            return Ok(true);
        }
        // Unpin of a disappeared selection is allowed, but cannot add out-of-scope rules.
        let entry = self.entries.get(target);
        if entry.is_none() && (pinned || !self.rules.contains_key(target)) {
            return Err(Error::Invalid(
                "File atau folder tidak ditemukan pada profil ini.".into(),
            ));
        }
        sqlx::query("INSERT INTO offline_selections(target,pinned) VALUES(?,?) ON CONFLICT(target) DO UPDATE SET pinned=excluded.pinned").bind(target).bind(pinned).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO offline_commands(request_id,payload) VALUES(?,?)")
            .bind(request_id)
            .bind(payload)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(false)
    }
}
fn ensure_directory(
    path: &str,
    entries: &mut HashMap<String, Entry>,
    directories: &mut HashMap<String, String>,
) -> String {
    if let Some(key) = directories.get(path) {
        return key.clone();
    }
    let mut parent = "root".to_string();
    let mut current = String::new();
    for component in path.split('/').filter(|s| !s.is_empty()) {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(component);
        if let Some(key) = directories.get(&current) {
            parent = key.clone();
            continue;
        }
        let key = format!("p:{current}");
        entries.insert(
            key.clone(),
            node(key.clone(), component.into(), true, Some(parent)),
        );
        directories.insert(current.clone(), key.clone());
        parent = key;
    }
    parent
}
fn present(root: &Path, file: &LocalFile) -> bool {
    let Some(fp) = &file.fingerprint else {
        return false;
    };
    let Ok(path) = local::safe_path(root, &file.relative_path) else {
        return false;
    };
    let Ok(m) = std::fs::symlink_metadata(path) else {
        return false;
    };
    m.is_file()
        && m.dev() == fp.device
        && m.ino() == fp.inode
        && m.len() == fp.digest.size
        && m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128 == fp.modified_ns
        && m.ctime() as i128 * 1_000_000_000 + m.ctime_nsec() as i128 == fp.changed_ns
}
