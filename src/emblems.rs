//! Lazy GVfs emblem updates. Never call GIO from the FUSE dispatcher.
use crate::availability::FolderBadge;
use gio::prelude::*;
use std::{
    collections::{HashMap, VecDeque},
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, RwLock,
    },
    time::Duration,
};

pub(crate) type Badges = Arc<RwLock<Arc<HashMap<String, FolderBadge>>>>;
pub(crate) struct Worker {
    requests: mpsc::SyncSender<String>,
    stop: Arc<AtomicBool>,
}
fn merged(previous: &[String], badge: FolderBadge) -> Vec<String> {
    let mut next: Vec<_> = previous
        .iter()
        .filter(|s| {
            !matches!(
                s.as_str(),
                "mygdrive-online" | "mygdrive-offline-ready" | "mygdrive-offline-preparing"
            )
        })
        .cloned()
        .collect();
    next.push(badge.icon().into());
    next
}
fn apply(path: &Path, badge: FolderBadge) -> std::result::Result<(), gio::glib::Error> {
    let file = gio::File::for_path(path);
    let info = file.query_info(
        "metadata::emblems",
        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
        gio::Cancellable::NONE,
    )?;
    let previous: Vec<String> = if info.has_attribute("metadata::emblems") {
        info.attribute_stringv("metadata::emblems")
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        Vec::new()
    };
    let next = merged(&previous, badge);
    if next != previous {
        let update = gio::FileInfo::new();
        update.set_attribute_stringv(
            "metadata::emblems",
            next.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        file.set_attributes_from_info(
            &update,
            gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            gio::Cancellable::NONE,
        )?;
    }
    // GVfs metadata alone does not emit an inotify event. A managed virtual
    // xattr generates IN_ATTRIB, so Thunar reloads this folder's emblem without
    // touching backing contents, timestamps, or starting a watcher scan.
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        gio::glib::Error::new(gio::IOErrorEnum::InvalidArgument, "invalid emblem path")
    })?;
    let name = c"user.mygdrive.emblem";
    let value = badge.icon().as_bytes();
    if unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    } != 0
    {
        return Err(gio::glib::Error::new(
            gio::IOErrorEnum::Failed,
            "emblem refresh deferred",
        ));
    }
    Ok(())
}
impl Worker {
    pub(crate) fn start(state: PathBuf, mount: PathBuf, badges: Badges) -> std::io::Result<Self> {
        let (requests, events) = mpsc::sync_channel::<String>(256);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        std::thread::Builder::new()
            .name("mygdrive-emblems".into())
            .spawn(move || {
                let mut seen = VecDeque::<String>::new();
                let mut applied = HashMap::<String, FolderBadge>::new();
                while !worker_stop.load(Ordering::Relaxed) {
                    let mut pending = Vec::new();
                    match events.recv_timeout(Duration::from_millis(500)) {
                        Ok(path) => pending.push(path),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    pending.extend(events.try_iter().take(255));
                    for path in pending {
                        if Path::new(&path)
                            .components()
                            .any(|c| !matches!(c, Component::Normal(_)))
                        {
                            continue;
                        }
                        if !seen.contains(&path) {
                            if seen.len() == 1024 {
                                if let Some(old) = seen.pop_front() {
                                    applied.remove(&old);
                                }
                            }
                            seen.push_back(path);
                        }
                    }
                    if seen.is_empty() {
                        continue;
                    }
                    if crate::cloud_view::mounted_online_root(&state).as_deref()
                        != Some(mount.as_path())
                    {
                        break;
                    }
                    // Drain a pending namespace snapshot even when Thunar is idle.
                    // This is metadata-only; never open or hydrate file contents.
                    let _ = std::fs::symlink_metadata(&mount);
                    // Clone only the Arc; no account-wide map copy per lookup.
                    let current = badges.read().unwrap().clone();
                    for path in &seen {
                        if worker_stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let Some(badge) = current.get(path).copied() else {
                            continue;
                        };
                        if applied.get(path) == Some(&badge) {
                            continue;
                        }
                        match apply(&mount.join(path), badge) {
                            Ok(()) => {
                                applied.insert(path.clone(), badge);
                            }
                            Err(_) => {
                                // Retry later. Missing GVfs must not break the drive.
                                std::thread::sleep(Duration::from_millis(100));
                                break;
                            }
                        }
                    }
                }
            })?;
        Ok(Self { requests, stop })
    }
    pub(crate) fn observed(&self, path: &str) {
        let _ = self.requests.try_send(path.to_string());
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changes_replace_only_managed_emblems() {
        let before = vec!["emblem-favorite".into(), "mygdrive-online".into()];
        let ready = merged(&before, FolderBadge::Offline);
        assert_eq!(ready, ["emblem-favorite", "mygdrive-offline-ready"]);
        assert_eq!(merged(&ready, FolderBadge::Offline), ready);
        assert_eq!(merged(&ready, FolderBadge::Online), before);
    }
}
