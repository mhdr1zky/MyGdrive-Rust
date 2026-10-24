//! Context-menu commands are clients of the existing single-writer service.
use crate::{
    cloud_view::{relative_selection, View},
    db::Database,
    offline::{Graph, Request as OfflineRequest},
    service::{self, Action, Request},
    Error, Result,
};
use std::path::{Path, PathBuf};
pub async fn set_paths(
    state: &Path,
    account: &str,
    mount: Option<&Path>,
    paths: &[PathBuf],
    pinned: bool,
) -> Result<usize> {
    if paths.is_empty() || paths.len() > 256 {
        return Err(Error::Invalid(
            "Pilih 1–256 file/folder MyGdrive Rust.".into(),
        ));
    }
    let db = Database::read_only(&state.join("state.sqlite3"), account).await?;
    let view = View::load(&db).await?;
    let graph = Graph::load(&db, false).await?;
    let mut targets = Vec::new();
    for path in paths {
        if !path.is_absolute() {
            return Err(Error::Invalid("Thunar must pass absolute paths".into()));
        }
        let relative = if let Some(mount) = mount.filter(|m| path.starts_with(m)) {
            relative_selection(mount, path)?
        } else {
            relative_selection(&view.root, path)?
        };
        let entry = view.target(&relative)?;
        let node = graph.selection(&entry.key).ok_or_else(|| {
            Error::Invalid("File belum tercatat. Tunggu pemindaian lokal lalu coba lagi.".into())
        })?;
        if !pinned && node.inherited {
            return Err(Error::Invalid("Pilihan offline diwarisi dari folder induk. Lepaskan pilihan pada folder induknya.".into()));
        }
        if !targets.contains(&entry.key) {
            targets.push(entry.key.clone());
        }
    }
    // Validate the complete selection before changing any rule.
    let status = service::send(state, &Request::new(Action::Status)).await?;
    for target in &targets {
        let mut r = Request::new(Action::OfflineSet);
        r.session_id = Some(status.status.session_id.clone());
        r.offline = Some(OfflineRequest::Set {
            target: target.clone(),
            pinned,
        });
        let reply = service::send(state, &r).await?;
        if !reply.accepted {
            return Err(Error::Invalid(
                "Layanan menolak pilihan. Muat ulang status dan coba lagi.".into(),
            ));
        }
    }
    db.pool.close().await;
    Ok(targets.len())
}
