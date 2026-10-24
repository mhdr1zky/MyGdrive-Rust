//! Folder pin intent and verified local availability, without network I/O.
use crate::{cloud_view::Entry, model::LocalFile};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderBadge {
    Online,
    Preparing,
    Offline,
}
impl FolderBadge {
    pub fn icon(self) -> &'static str {
        match self {
            Self::Online => "mygdrive-online",
            Self::Preparing => "mygdrive-offline-preparing",
            Self::Offline => "mygdrive-offline-ready",
        }
    }
}
fn parent(path: &str) -> &str {
    Path::new(path)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or("")
}
pub fn folder_badges(
    entries: &BTreeMap<String, Entry>,
    files: &[LocalFile],
    rules: &HashMap<String, bool>,
    present: &HashSet<String>,
    inventory_complete: bool,
) -> HashMap<String, FolderBadge> {
    let ready = files
        .iter()
        .filter(|f| {
            f.materialized
                && f.fingerprint.is_some()
                && !f.review_required
                && present.contains(&f.relative_path)
        })
        .map(|f| f.relative_path.clone())
        .collect();
    folder_badges_from_ready(entries, rules, &ready, inventory_complete)
}

pub(crate) fn folder_badges_from_ready(
    entries: &BTreeMap<String, Entry>,
    rules: &HashMap<String, bool>,
    ready_paths: &HashSet<String>,
    inventory_complete: bool,
) -> HashMap<String, FolderBadge> {
    let mut effective = HashMap::<String, bool>::new();
    for (path, entry) in entries.iter().filter(|(_, e)| e.folder) {
        let selected = rules
            .get(&entry.key)
            .copied()
            .unwrap_or_else(|| rules.get(&format!("p:{path}")).copied().unwrap_or(false));
        let inherited = !path.is_empty() && effective.get(parent(path)).copied().unwrap_or(false);
        effective.insert(path.clone(), selected || inherited);
    }
    let mut counts = HashMap::<String, (u64, u64)>::new();
    let mut badges = HashMap::new();
    // Reverse path order visits descendants before their parent. Each entry is
    // aggregated once; no recursive SQL/graph reload for each folder.
    for (path, entry) in entries.iter().rev() {
        let (total, ready) = if entry.folder {
            counts.get(path).copied().unwrap_or_default()
        } else {
            let ready = !entry.unsupported && ready_paths.contains(path);
            (1, u64::from(ready))
        };
        if entry.folder {
            let badge = if !effective.get(path).copied().unwrap_or(false) {
                FolderBadge::Online
            } else if inventory_complete && total == ready {
                FolderBadge::Offline
            } else {
                FolderBadge::Preparing
            };
            badges.insert(path.clone(), badge);
        }
        if !path.is_empty() {
            let parent = counts.entry(parent(path).to_string()).or_default();
            parent.0 += total;
            parent.1 += ready;
        }
    }
    badges
}
