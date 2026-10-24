use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    pub sha256: String,
    pub md5: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub digest: Digest,
    pub device: u64,
    pub inode: u64,
    pub modified_ns: i128,
    pub changed_ns: i128,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub can_download: Option<bool>,
    pub can_edit: Option<bool>,
    pub can_trash: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFile {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub version: String,
    pub size: Option<String>,
    pub sha256_checksum: Option<String>,
    pub md5_checksum: Option<String>,
    pub head_revision_id: Option<String>,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub trashed: bool,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// A change removal may mean permission loss, not deletion.
    #[serde(default)]
    pub unavailable: bool,
}

impl RemoteFile {
    pub fn folder(&self) -> bool {
        self.mime_type == "application/vnd.google-apps.folder"
    }
    pub fn workspace(&self) -> bool {
        self.mime_type.starts_with("application/vnd.google-apps.") && !self.folder()
    }
    pub fn matches(&self, digest: &Digest) -> Option<bool> {
        if let Some(hash) = &self.sha256_checksum {
            return Some(hash.eq_ignore_ascii_case(&digest.sha256));
        }
        self.md5_checksum
            .as_ref()
            .map(|hash| hash.eq_ignore_ascii_case(&digest.md5))
    }
    pub fn metadata_eq(&self, other: &Self) -> bool {
        self.name == other.name && self.parents == other.parents
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub digest: Digest,
    pub relative_path: String,
    pub remote: RemoteFile,
    /// Drive projection before resolving local name collisions. Distinct from the local alias.
    #[serde(default)]
    pub remote_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalFile {
    pub id: String,
    pub relative_path: String,
    pub remote_id: Option<String>,
    pub generation: i64,
    pub fingerprint: Option<Fingerprint>,
    pub baseline: Option<Baseline>,
    pub status: String,
    pub materialized: bool,
    #[serde(default)]
    pub review_required: bool,
    #[serde(default)]
    pub available_offline: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveJournal {
    pub source: String,
    pub target: String,
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone)]
pub struct CachedObject {
    pub key: String,
    pub snapshot: Snapshot,
    pub remote_id: Option<String>,
    pub last_access_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub path: PathBuf,
    pub digest: Digest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallJournal {
    pub target: PathBuf,
    pub staging: PathBuf,
    pub incoming_inode: u64,
    pub old_inode: Option<u64>,
    pub old_device: Option<u64>,
    pub old_digest: Option<Digest>,
    pub incoming_digest: Digest,
    pub exchanged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub key: String,
    pub file_id: String,
    pub generation: i64,
    pub kind: String,
    pub stage: String,
    pub remote: Option<RemoteFile>,
    pub snapshot: Option<Snapshot>,
    pub downloaded: Option<Snapshot>,
    pub target_remote_id: Option<String>,
    pub conflict_path: Option<String>,
    pub conflict_file_id: Option<String>,
    pub install: Option<InstallJournal>,
    #[serde(default)]
    pub projected_path: Option<String>,
    #[serde(default)]
    pub move_journal: Option<MoveJournal>,
    #[serde(default)]
    pub local_path: Option<String>,
    pub attempts: u32,
    pub next_retry_ms: i64,
    pub last_error: Option<String>,
}

impl Operation {
    pub fn new(file: &LocalFile, kind: &str, remote: Option<RemoteFile>) -> Self {
        let version = remote.as_ref().map(|r| r.version.as_str()).unwrap_or("new");
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            key: format!("{}:{}:{kind}:{version}", file.id, file.generation),
            file_id: file.id.clone(),
            generation: file.generation,
            kind: kind.into(),
            stage: "planned".into(),
            remote,
            snapshot: None,
            downloaded: None,
            target_remote_id: None,
            conflict_path: None,
            conflict_file_id: None,
            install: None,
            projected_path: None,
            move_journal: None,
            local_path: Some(file.relative_path.clone()),
            attempts: 0,
            next_retry_ms: 0,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplacedFile {
    pub path: PathBuf,
    pub original_relative_path: String,
    pub expected: Digest,
    pub incoming_inode: u64,
    pub promoted_sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub file_id: String,
    #[serde(default)]
    pub removed: bool,
    pub file: Option<RemoteFile>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangePage {
    #[serde(default)]
    pub changes: Vec<Change>,
    pub next_page_token: Option<String>,
    pub new_start_page_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilePage {
    #[serde(default)]
    pub files: Vec<RemoteFile>,
    pub next_page_token: Option<String>,
}
