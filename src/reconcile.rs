//! Three-way decisions. No I/O and no wall-clock-based last-writer-wins.
use crate::model::{LocalFile, RemoteFile};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Nothing,
    Adopt,
    Create,
    Upload,
    Download,
    Conflict,
    Rename,
    Trash,
    RemoteTrash,
    DeleteEditConflict,
    Unavailable,
    NeedRemoteHash,
    MetadataConflict,
    Unsupported,
}

pub fn reconcile(local: &LocalFile, remote: Option<&RemoteFile>) -> Decision {
    use Decision::*;
    let Some(remote) = remote else {
        return if local.remote_id.is_some() {
            Unavailable
        } else if local.fingerprint.is_some() {
            Create
        } else {
            Nothing
        };
    };
    if remote.unavailable {
        return Unavailable;
    }
    if remote.workspace() || remote.folder() {
        return Unsupported;
    }
    let Some(base) = &local.baseline else {
        return if remote.trashed { Nothing } else { Download };
    };
    let local_content_changed = local.fingerprint.as_ref().map(|f| &f.digest) != Some(&base.digest);
    let remote_content_changed = if remote.trashed {
        true
    } else if remote.version == base.remote.version {
        false
    } else {
        match remote.matches(&base.digest) {
            Some(equal) => !equal,
            None => return NeedRemoteHash,
        }
    };
    let local_moved = local.relative_path != base.relative_path;
    let remote_moved = !remote.metadata_eq(&base.remote);

    if local.fingerprint.is_none() && !local.materialized && local_moved {
        return if remote.trashed || remote_content_changed || remote_moved {
            MetadataConflict
        } else {
            Rename
        };
    }
    if local.fingerprint.is_none() {
        if remote.trashed {
            return Adopt;
        }
        if !local.materialized {
            return Download;
        }
        return if remote_content_changed || remote_moved {
            DeleteEditConflict
        } else {
            Trash
        };
    }
    if remote.trashed {
        return if local_content_changed || local_moved {
            DeleteEditConflict
        } else {
            RemoteTrash
        };
    }
    if remote.matches(&local.fingerprint.as_ref().unwrap().digest) == Some(true) {
        if local_moved && remote_moved {
            return Conflict;
        }
        if local_moved || remote_moved {
            return Rename;
        }
        return Adopt;
    }
    if local_content_changed && remote_content_changed {
        return Conflict;
    }
    if local_moved && remote_moved {
        return Conflict;
    }
    if local_content_changed {
        Upload
    } else if remote_content_changed {
        Download
    } else if local_moved || remote_moved {
        Rename
    } else if remote.version != base.remote.version {
        Adopt
    } else {
        Nothing
    }
}
