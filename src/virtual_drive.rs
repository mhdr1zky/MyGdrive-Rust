//! User-owned FUSE projection. Metadata listing never reads remote content.
//! All writable bytes go through the ordinary watched backing root.
use crate::{
    cloud_view::{Entry, View},
    db::Database,
    local,
    service::{self, Action, Request},
    Error, Result,
};
use fuser::*;
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{File, OpenOptions},
    os::fd::AsRawFd,
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, RwLock,
    },
    time::{Duration, Instant, SystemTime},
};

const TTL: Duration = Duration::from_millis(250);
// File managers sniff contents and thumbnailers read files just to display a
// folder. Those speculative reads must not queue hydration behind a sync cycle.
// Explicit opens in an editor still use the normal verified download path.
fn process_named(pid: u32, names: &[&str]) -> bool {
    let known = |name: &str| names.contains(&name);
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| known(&n.to_string_lossy())))
        .unwrap_or(false)
        || std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|name| known(name.trim()))
}
fn errno(e: Error) -> i32 {
    match e {
        Error::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
        Error::Changed => libc::EAGAIN,
        Error::Unsupported(_) => libc::EOPNOTSUPP,
        Error::AuthRequired => libc::EACCES,
        _ => libc::EIO,
    }
}
struct OpenFile {
    file: File,
    writable: bool,
    entry: Entry,
}
enum Mutation {
    Removed(String),
    Renamed(String, String),
}

// Keep SQLite reads and full-account projection out of the FUSE dispatch loop.
// A bounded mailbox retains the last usable namespace during slow refreshes.
struct NamespaceUpdates {
    receiver: mpsc::Receiver<(u64, View)>,
    epoch: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    pending: Arc<AtomicBool>,
    badges: crate::emblems::Badges,
}
impl NamespaceUpdates {
    fn start(
        state: &Path,
        account: &str,
        initial: HashMap<String, crate::availability::FolderBadge>,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let epoch = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(AtomicBool::new(false));
        let badges = Arc::new(RwLock::new(Arc::new(initial)));
        let worker_badges = badges.clone();
        let state = state.to_path_buf();
        let account = account.to_string();
        let worker_epoch = epoch.clone();
        let worker_stop = stop.clone();
        let worker_pending = pending.clone();
        std::thread::Builder::new()
            .name("mygdrive-namespace".into())
            .spawn(move || {
                let run = || -> Result<()> {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    let db =
                        rt.block_on(Database::read_only(&state.join("state.sqlite3"), &account))?;
                    let mut version = None;
                    let mut previous: Option<(u64, View)> = None;
                    let mut refreshed = Instant::now();
                    while !worker_stop.load(Ordering::Relaxed) {
                        let generation = worker_epoch.load(Ordering::Acquire);
                        let result = rt.block_on(async {
                            let current: i64 = sqlx::query_scalar("PRAGMA data_version")
                                .fetch_one(&db.pool)
                                .await?;
                            if !worker_pending.load(Ordering::Acquire)
                                && (version != Some(current)
                                    || refreshed.elapsed() >= Duration::from_secs(30))
                            {
                                let view = View::load(&db).await?;
                                if generation == worker_epoch.load(Ordering::Acquire)
                                    && worker_badges.read().unwrap().as_ref() != &view.folder_badges
                                {
                                    *worker_badges.write().unwrap() =
                                        Arc::new(view.folder_badges.clone());
                                }
                                // Scanner/status commits often leave the namespace unchanged.
                                // Do not rebuild 80k inodes on the FUSE thread for those commits.
                                if previous.as_ref().is_some_and(|(epoch, last)| {
                                    *epoch == generation
                                        && last.root == view.root
                                        && last.entries == view.entries
                                }) {
                                    version = Some(current);
                                    refreshed = Instant::now();
                                    return Ok(true);
                                }
                                let next_previous = view.clone();
                                worker_pending.store(true, Ordering::Release);
                                match sender.try_send((generation, view)) {
                                    Ok(()) => {
                                        version = Some(current);
                                        previous = Some((generation, next_previous));
                                        refreshed = Instant::now();
                                    }
                                    Err(mpsc::TrySendError::Full(_)) => {}
                                    Err(mpsc::TrySendError::Disconnected(_)) => return Ok(false),
                                }
                            }
                            Ok::<_, Error>(true)
                        });
                        match result {
                            Ok(false) => break,
                            Ok(true) => {}
                            Err(error) => {
                                tracing::warn!(%error, "keeping previous online namespace")
                            }
                        }
                        crate::memory::release_unused();
                        for _ in 0..4 {
                            if worker_stop.load(Ordering::Relaxed) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                    Ok(())
                };
                if let Err(error) = run() {
                    tracing::warn!(%error, "namespace observer stopped");
                }
            })?;
        Ok(Self {
            receiver,
            epoch,
            stop,
            pending,
            badges,
        })
    }
}
impl Drop for NamespaceUpdates {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
pub struct DriveFs {
    runtime: tokio::runtime::Runtime,
    db: Database,
    state: PathBuf,
    view: View,
    inodes: HashMap<String, u64>,
    nodes: HashMap<u64, Entry>,
    next_inode: u64,
    files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    pending_opens: Arc<tokio::sync::Semaphore>,
    directories: HashMap<u64, Vec<(u64, FileType, String)>>,
    next_handle: u64,
    updates: NamespaceUpdates,
    children: HashMap<String, Vec<String>>,
    mutations: mpsc::Receiver<Mutation>,
    mutation_sender: mpsc::Sender<Mutation>,
    mutation_gate: Arc<tokio::sync::Mutex<()>>,
    emblems: Option<crate::emblems::Worker>,
    emblem_attributes: HashMap<u64, Vec<u8>>,
}
impl DriveFs {
    pub fn new(state: &Path, account: &str) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(8)
            .enable_all()
            .build()?;
        let db = runtime.block_on(Database::read_only(&state.join("state.sqlite3"), account))?;
        let view = runtime.block_on(View::load(&db))?;
        let badges = view.folder_badges.clone();
        let (mutation_sender, mutations) = mpsc::channel();
        let mut me = Self {
            runtime,
            db,
            state: state.into(),
            view,
            inodes: HashMap::from([("root".into(), 1)]),
            nodes: HashMap::new(),
            next_inode: 2,
            files: Arc::new(Mutex::new(HashMap::new())),
            pending_opens: Arc::new(tokio::sync::Semaphore::new(32)),
            directories: HashMap::new(),
            next_handle: 1,
            updates: NamespaceUpdates::start(state, account, badges)?,
            children: HashMap::new(),
            mutations,
            mutation_sender,
            mutation_gate: Arc::new(tokio::sync::Mutex::new(())),
            emblems: None,
            emblem_attributes: HashMap::new(),
        };
        me.reindex();
        Ok(me)
    }
    pub fn backing_root(&self) -> &Path {
        &self.view.root
    }
    pub fn enable_emblems(&mut self, mount: &Path) -> Result<()> {
        self.emblems = Some(crate::emblems::Worker::start(
            self.state.clone(),
            mount.to_path_buf(),
            self.updates.badges.clone(),
        )?);
        Ok(())
    }
    fn reindex(&mut self) {
        self.nodes.clear();
        self.children.clear();
        for e in self.view.entries.values() {
            if !e.path.is_empty() {
                let parent = Path::new(&e.path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("");
                self.children
                    .entry(parent.to_string())
                    .or_default()
                    .push(e.path.clone());
            }
            let next = &mut self.next_inode;
            let ino = *self.inodes.entry(e.key.clone()).or_insert_with(|| {
                let n = *next;
                *next += 1;
                n
            });
            self.nodes.insert(ino, e.clone());
        }
    }
    fn refresh(&mut self, force: bool) -> Result<()> {
        while let Ok(mutation) = self.mutations.try_recv() {
            match mutation {
                Mutation::Removed(path) => self.publish_removed(&path),
                Mutation::Renamed(from, to) => self.publish_rename(&from, &to),
            }
        }
        if force {
            // Discard snapshots begun before our local namespace mutation.
            self.updates.epoch.fetch_add(1, Ordering::AcqRel);
            self.view = self.runtime.block_on(View::load(&self.db))?;
            self.reindex();
        } else if let Ok((generation, view)) = self.updates.receiver.try_recv() {
            self.updates.pending.store(false, Ordering::Release);
            if generation == self.updates.epoch.load(Ordering::Acquire) {
                self.view = view;
                self.reindex();
            }
        }
        Ok(())
    }
    fn put_entry(&mut self, entry: Entry, preferred_inode: Option<u64>) {
        let inode = preferred_inode.unwrap_or_else(|| {
            let inode = self.next_inode;
            self.next_inode += 1;
            inode
        });
        let parent = Path::new(&entry.path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        let children = self.children.entry(parent.to_string()).or_default();
        if !children.contains(&entry.path) {
            children.push(entry.path.clone());
            children.sort();
        }
        self.inodes.insert(entry.key.clone(), inode);
        self.nodes.insert(inode, entry.clone());
        Arc::make_mut(&mut self.view.entries).insert(entry.path.clone(), entry);
    }
    fn forget_path(&mut self, path: &str) {
        if let Some(entry) = Arc::make_mut(&mut self.view.entries).remove(path) {
            if let Some(inode) = self.inodes.remove(&entry.key) {
                self.nodes.remove(&inode);
            }
            let parent = Path::new(path)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("");
            if let Some(children) = self.children.get_mut(parent) {
                children.retain(|child| child != path);
            }
            self.children.remove(path);
        }
    }
    fn publish_created(&mut self, path: &str, folder: bool) {
        self.updates.epoch.fetch_add(1, Ordering::AcqRel);
        self.put_entry(
            Entry {
                path: path.into(),
                key: format!("p:{path}"),
                folder,
                size: 0,
                file_id: None,
                unsupported: false,
            },
            None,
        );
    }
    fn publish_removed(&mut self, path: &str) {
        self.updates.epoch.fetch_add(1, Ordering::AcqRel);
        self.forget_path(path);
    }
    fn publish_rename(&mut self, source: &str, destination: &str) {
        self.updates.epoch.fetch_add(1, Ordering::AcqRel);
        let Some(source_entry) = self.view.entries.get(source).cloned() else {
            return;
        };
        let prefix = format!("{source}/");
        let mut moved = vec![source_entry];
        moved.extend(
            self.view
                .entries
                .range(prefix.clone()..)
                .take_while(|(path, _)| path.starts_with(&prefix))
                .map(|(_, e)| e.clone()),
        );
        let moved: Vec<_> = moved
            .into_iter()
            .map(|e| {
                let inode = self.inodes[&e.key];
                (e, inode)
            })
            .collect();
        for (entry, _) in &moved {
            self.forget_path(&entry.path);
        }
        self.forget_path(destination);
        for (mut entry, inode) in moved {
            entry.path = format!("{destination}{}", &entry.path[source.len()..]);
            if entry.key.starts_with("p:") {
                entry.key = format!("p:{}", entry.path);
            }
            self.put_entry(entry, Some(inode));
        }
    }
    fn entry(&mut self, ino: u64) -> Result<Entry> {
        self.refresh(false)?;
        let entry = self
            .nodes
            .get(&ino)
            .cloned()
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
        if entry.folder {
            if let Some(worker) = &self.emblems {
                worker.observed(&entry.path);
            }
        }
        Ok(entry)
    }
    fn child(&mut self, parent: u64, name: &OsStr) -> Result<String> {
        let p = self.entry(parent)?;
        if !p.folder {
            return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR).into());
        }
        let n = name
            .to_str()
            .filter(|s| {
                !s.is_empty()
                    && !s.contains('/')
                    && *s != "."
                    && *s != ".."
                    && !s.starts_with(local::INTERNAL_PREFIX)
                    && !s.starts_with(".Trash")
            })
            .ok_or_else(|| Error::Invalid("invalid filename".into()))?;
        Ok(if p.path.is_empty() {
            n.into()
        } else {
            format!("{}/{n}", p.path)
        })
    }
    fn path(&self, e: &Entry) -> Result<PathBuf> {
        if e.path.is_empty() {
            Ok(self.view.root.clone())
        } else {
            local::safe_path(&self.view.root, &e.path)
        }
    }
    async fn hydrate_file(state: &Path, e: &Entry, path: &Path) -> Result<()> {
        if path.try_exists()? {
            return Ok(());
        }
        if e.unsupported {
            return Err(Error::Unsupported(
                "Google Workspace or download denied".into(),
            ));
        }
        let id = e.file_id.as_ref().ok_or(Error::Changed)?;
        let status = service::send(state, &Request::new(Action::Status)).await?;
        let mut request = Request::new(Action::Hydrate);
        request.session_id = Some(status.status.session_id);
        request.offline = Some(crate::offline::Request::Hydrate {
            file_id: id.clone(),
        });
        if !service::send(state, &request).await?.accepted {
            return Err(Error::Changed);
        }
        Ok(())
    }
    // Eviction takes an exclusive flock before moving a backing file out of
    // the namespace. A shared lease lives on the open file description, so
    // try_clone() also keeps pending reads, writes and close-time fsync safe.
    fn protect_materialized_file(path: &Path, file: File) -> Result<Option<File>> {
        match fs2::FileExt::try_lock_shared(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let opened = file.metadata()?;
        if !opened.is_file() {
            return Err(Error::Unsupported("not a regular file".into()));
        }
        let current = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        // An eviction may have won between open() and flock(). Never expose
        // an unlinked/quarantined inode, especially through a writable handle.
        if opened.nlink() == 0
            || !current.is_file()
            || opened.dev() != current.dev()
            || opened.ino() != current.ino()
        {
            return Ok(None);
        }
        Ok(Some(file))
    }
    fn try_open_materialized_file(path: &Path, flags: i32) -> Result<Option<File>> {
        let writable = flags & libc::O_ACCMODE != libc::O_RDONLY;
        let file = match OpenOptions::new()
            .read(flags & libc::O_ACCMODE != libc::O_WRONLY)
            .write(writable)
            .append(writable && flags & libc::O_APPEND != 0)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(file) = Self::protect_materialized_file(path, file)? else {
            return Ok(None);
        };
        // O_TRUNC in open() would modify bytes before obtaining the lease.
        if writable && flags & libc::O_TRUNC != 0 {
            file.set_len(0)?;
        }
        Ok(Some(file))
    }
    async fn open_materialized_file(
        state: &Path,
        entry: &Entry,
        path: &Path,
        flags: i32,
        allow_hydration: bool,
    ) -> Result<File> {
        let mut retry_started = None;
        loop {
            if !path.try_exists()? {
                if !allow_hydration {
                    return Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP).into());
                }
                if let Err(error) = Self::hydrate_file(state, entry, path).await {
                    if !matches!(error, Error::Changed) {
                        return Err(error);
                    }
                }
            }
            if let Some(file) = Self::try_open_materialized_file(path, flags)? {
                return Ok(file);
            }
            if retry_started.get_or_insert_with(Instant::now).elapsed() >= Duration::from_secs(5) {
                return Err(Error::Changed);
            }
            // Retry a missing/replaced path or a short eviction lease without
            // blocking FUSE dispatch or retaining a stale backing descriptor.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn metadata_intent(
        state: &Path,
        entry: &Entry,
        destination: Option<String>,
    ) -> Result<()> {
        let status = service::send(state, &Request::new(Action::Status)).await?;
        let mut request = Request::new(Action::OfflineSet);
        request.session_id = Some(status.status.session_id);
        request.offline = Some(crate::offline::Request::Metadata {
            file_id: entry.file_id.clone().ok_or(Error::Changed)?,
            source: entry.path.clone(),
            destination,
        });
        if !service::send(state, &request).await?.accepted {
            return Err(Error::Changed);
        }
        Ok(())
    }
    fn clone_handle(&self, handle: u64, writable: bool) -> std::io::Result<File> {
        let files = self.files.lock().unwrap();
        let file = files
            .get(&handle)
            .filter(|f| !writable || f.writable)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EBADF))?;
        file.file.try_clone()
    }
    fn attr(&self, ino: u64, e: &Entry, fh: Option<u64>) -> Result<FileAttr> {
        let handle = fh.and_then(|h| {
            self.files
                .lock()
                .unwrap()
                .get(&h)
                .map(|f| f.file.try_clone())
        });
        let metadata = if let Some(file) = handle {
            Some(file?.metadata()?)
        } else {
            match std::fs::symlink_metadata(self.path(e)?) {
                Ok(m) if m.is_symlink() => {
                    return Err(Error::Invalid("symlink in backing root".into()))
                }
                Ok(m) => Some(m),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => return Err(err.into()),
            }
        };
        Ok(Self::file_attr(ino, e, metadata.as_ref()))
    }
    fn file_attr(ino: u64, e: &Entry, metadata: Option<&std::fs::Metadata>) -> FileAttr {
        let size = metadata.as_ref().map(|m| m.len()).unwrap_or(e.size);
        let modified = metadata
            .as_ref()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        FileAttr {
            ino,
            size: if e.folder { 0 } else { size },
            blocks: size.div_ceil(512),
            atime: modified,
            mtime: modified,
            ctime: modified,
            crtime: SystemTime::UNIX_EPOCH,
            kind: if e.folder {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if e.folder { 0o700 } else { 0o600 },
            nlink: if e.folder { 2 } else { 1 },
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}
impl Filesystem for DriveFs {
    fn setxattr(
        &mut self,
        _: &fuser::Request<'_>,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        if name != OsStr::new("user.mygdrive.emblem") {
            reply.error(libc::EOPNOTSUPP);
            return;
        }
        if position != 0
            || flags & !(libc::XATTR_CREATE | libc::XATTR_REPLACE) != 0
            || !matches!(
                value,
                b"mygdrive-online" | b"mygdrive-offline-ready" | b"mygdrive-offline-preparing"
            )
        {
            reply.error(libc::EINVAL);
            return;
        }
        match self.entry(ino) {
            Ok(entry) if entry.folder => {}
            Ok(_) => {
                reply.error(libc::EOPNOTSUPP);
                return;
            }
            Err(error) => {
                reply.error(errno(error));
                return;
            }
        }
        let exists = self.emblem_attributes.contains_key(&ino);
        if flags & libc::XATTR_CREATE != 0 && exists {
            reply.error(libc::EEXIST);
            return;
        }
        if flags & libc::XATTR_REPLACE != 0 && !exists {
            reply.error(libc::ENODATA);
            return;
        }
        self.emblem_attributes.insert(ino, value.to_vec());
        reply.ok();
    }
    fn getxattr(
        &mut self,
        _: &fuser::Request<'_>,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        if name != OsStr::new("user.mygdrive.emblem") {
            reply.error(libc::ENODATA);
            return;
        }
        match self.emblem_attributes.get(&ino) {
            Some(value) if size == 0 => reply.size(value.len() as u32),
            Some(value) if (size as usize) >= value.len() => reply.data(value),
            Some(_) => reply.error(libc::ERANGE),
            None => reply.error(libc::ENODATA),
        }
    }
    fn statfs(&mut self, _: &fuser::Request<'_>, _ino: u64, reply: ReplyStatfs) {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(self.view.root.as_os_str().as_bytes()) else {
            reply.error(libc::EINVAL);
            return;
        };
        let mut data = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::statvfs(path.as_ptr(), data.as_mut_ptr()) } != 0 {
            reply.error(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            );
            return;
        }
        let data = unsafe { data.assume_init() };
        reply.statfs(
            data.f_blocks,
            data.f_bfree,
            data.f_bavail,
            data.f_files,
            data.f_ffree,
            data.f_bsize as u32,
            data.f_namemax as u32,
            data.f_frsize as u32,
        );
    }
    fn lookup(&mut self, _: &fuser::Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let result = (|| {
            let path = self.child(parent, name)?;
            let e = self.view.target(&path)?.clone();
            if e.folder {
                if let Some(worker) = &self.emblems {
                    worker.observed(&e.path);
                }
            }
            let ino = self.inodes[&e.key];
            Ok((ino, e))
        })();
        match result.and_then(|(ino, e)| self.attr(ino, &e, None)) {
            Ok(a) => reply.entry(&TTL, &a, 0),
            Err(e) => reply.error(match e {
                Error::Invalid(_) => libc::ENOENT,
                _ => errno(e),
            }),
        }
    }
    fn getattr(&mut self, _: &fuser::Request<'_>, ino: u64, fh: Option<u64>, reply: ReplyAttr) {
        let opened_entry =
            fh.and_then(|h| self.files.lock().unwrap().get(&h).map(|f| f.entry.clone()));
        match opened_entry
            .map(Ok)
            .unwrap_or_else(|| self.entry(ino))
            .and_then(|e| self.attr(ino, &e, fh))
        {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn opendir(&mut self, _: &fuser::Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let r = (|| {
            let e = self.entry(ino)?;
            if !e.folder {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR).into());
            }
            let parent = Path::new(&e.path)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("");
            let parent_ino = self
                .view
                .entries
                .get(parent)
                .and_then(|p| self.inodes.get(&p.key))
                .copied()
                .unwrap_or(1);
            let mut children = vec![
                (ino, FileType::Directory, ".".into()),
                (parent_ino, FileType::Directory, "..".into()),
            ];
            for path in self.children.get(&e.path).into_iter().flatten() {
                let child = &self.view.entries[path];
                children.push((
                    self.inodes[&child.key],
                    if child.folder {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    },
                    Path::new(&child.path)
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into(),
                ));
            }
            let h = self.next_handle;
            self.next_handle += 1;
            self.directories.insert(h, children);
            Ok(h)
        })();
        match r {
            Ok(h) => reply.opened(h, 0),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn readdir(
        &mut self,
        _: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        match self.directories.get(&fh) {
            Some(entries) => {
                for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
                    if reply.add(*ino, (i + 1) as i64, *kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            None => reply.error(libc::EBADF),
        }
    }
    fn releasedir(
        &mut self,
        _: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        self.directories.remove(&fh);
        reply.ok();
    }
    fn open(&mut self, req: &fuser::Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let e = match self.entry(ino) {
            Ok(e) if !e.folder => e,
            Ok(_) => {
                reply.error(libc::EISDIR);
                return;
            }
            Err(e) => {
                reply.error(errno(e));
                return;
            }
        };
        let permit = match self.pending_opens.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                reply.error(libc::EAGAIN);
                return;
            }
        };
        let path = match self.path(&e) {
            Ok(path) => path,
            Err(e) => {
                reply.error(errno(e));
                return;
            }
        };
        let h = self.next_handle;
        self.next_handle += 1;
        let state = self.state.clone();
        let files = self.files.clone();
        let pid = req.pid();
        // A thumbnailer can open remote files while Thunar lists the directory.
        // Keep its network wait off the FUSE request dispatcher.
        self.runtime.spawn(async move {
            let _permit = permit;
            let result: Result<OpenFile> = async {
                let speculative = flags & libc::O_ACCMODE == libc::O_RDONLY
                    && (process_named(pid, &["tumblerd", "tumbler"])
                        || (flags & libc::O_NOATIME != 0
                            && process_named(pid, &["Thunar", "thunar"])));
                let writable = flags & libc::O_ACCMODE != libc::O_RDONLY;
                let file =
                    Self::open_materialized_file(&state, &e, &path, flags, !speculative).await?;
                Ok(OpenFile {
                    file,
                    writable,
                    entry: e,
                })
            }
            .await;
            match result {
                Ok(file) => {
                    files.lock().unwrap().insert(h, file);
                    reply.opened(h, 1);
                }
                Err(e) => reply.error(errno(e)),
            }
        });
    }
    fn read(
        &mut self,
        _: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let file = match self.clone_handle(fh, false) {
            Ok(file) => file,
            Err(error) => {
                reply.error(error.raw_os_error().unwrap_or(libc::EIO));
                return;
            }
        };
        self.runtime.spawn_blocking(move || {
            let mut bytes = vec![0; size.min(1024 * 1024) as usize];
            match file.read_at(&mut bytes, offset as u64) {
                Ok(n) => reply.data(&bytes[..n]),
                Err(error) => reply.error(error.raw_os_error().unwrap_or(libc::EIO)),
            }
        });
    }

    fn write(
        &mut self,
        _: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let file = match self.clone_handle(fh, true) {
            Ok(file) => file,
            Err(error) => {
                reply.error(error.raw_os_error().unwrap_or(libc::EIO));
                return;
            }
        };
        let bytes = data.to_vec();
        self.runtime
            .spawn_blocking(move || match file.write_at(&bytes, offset as u64) {
                Ok(n) => reply.written(n as u32),
                Err(error) => reply.error(error.raw_os_error().unwrap_or(libc::EIO)),
            });
    }

    fn flush(
        &mut self,
        _: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        // close() issues FLUSH even for read-only MIME/thumbnail reads. fsync on
        // those handles needlessly waits for concurrent sync writes to the disk.
        if self
            .files
            .lock()
            .unwrap()
            .get(&fh)
            .is_some_and(|f| !f.writable)
        {
            reply.ok();
            return;
        }
        let file = match self.clone_handle(fh, false) {
            Ok(file) => file,
            Err(error) => {
                reply.error(error.raw_os_error().unwrap_or(libc::EIO));
                return;
            }
        };
        self.runtime.spawn_blocking(move || match file.sync_all() {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error.raw_os_error().unwrap_or(libc::EIO)),
        });
    }

    fn fsync(
        &mut self,
        req: &fuser::Request<'_>,
        ino: u64,
        fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.flush(req, ino, fh, 0, reply);
    }
    fn release(
        &mut self,
        _: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let file = self.files.lock().unwrap().remove(&fh);
        self.runtime.spawn_blocking(move || match file {
            Some(file) if !file.writable => reply.ok(),
            Some(file) => match file.file.sync_all() {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(error.raw_os_error().unwrap_or(libc::EIO)),
            },
            None => reply.error(libc::EBADF),
        });
    }

    fn create(
        &mut self,
        _: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let r = (|| {
            let path = self.child(parent, name)?;
            if self.view.entries.contains_key(&path) {
                return Err(std::io::Error::from_raw_os_error(libc::EEXIST).into());
            }
            let real = local::safe_path(&self.view.root, &path)?;
            std::fs::create_dir_all(real.parent().unwrap())?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&real)?;
            let file = Self::protect_materialized_file(&real, file)?.ok_or(Error::Changed)?;
            file.sync_all()?;
            self.publish_created(&path, false);
            let e = self.view.target(&path)?.clone();
            let ino = self.inodes[&e.key];
            let attr = self.attr(ino, &e, None)?;
            let h = self.next_handle;
            self.next_handle += 1;
            self.files.lock().unwrap().insert(
                h,
                OpenFile {
                    file,
                    writable: flags & libc::O_ACCMODE != libc::O_RDONLY,
                    entry: e.clone(),
                },
            );
            Ok((attr, h))
        })();
        match r {
            Ok((a, h)) => reply.created(&TTL, &a, 0, h, 1),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn mkdir(
        &mut self,
        _: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let r = (|| {
            let path = self.child(parent, name)?;
            if self.view.entries.contains_key(&path) {
                return Err(std::io::Error::from_raw_os_error(libc::EEXIST).into());
            }
            let real = local::safe_path(&self.view.root, &path)?;
            std::fs::create_dir_all(real.parent().unwrap())?;
            std::fs::create_dir(&real)?;
            self.publish_created(&path, true);
            let e = self.view.target(&path)?;
            self.attr(self.inodes[&e.key], e, None)
        })();
        match r {
            Ok(a) => reply.entry(&TTL, &a, 0),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn unlink(&mut self, _: &fuser::Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let result = (|| -> Result<_> {
            let path = self.child(parent, name)?;
            let entry = self.view.target(&path)?.clone();
            if entry.folder {
                return Err(std::io::Error::from_raw_os_error(libc::EISDIR).into());
            }
            let real = self.path(&entry)?;
            Ok((path, entry, real))
        })();
        let (path, entry, real) = match result {
            Ok(value) => value,
            Err(error) => {
                reply.error(errno(error));
                return;
            }
        };
        let state = self.state.clone();
        let sender = self.mutation_sender.clone();
        let gate = self.mutation_gate.clone();
        let epoch = self.updates.epoch.clone();
        self.runtime.spawn(async move {
            let _guard = gate.lock().await;
            let result = async {
                if real.try_exists()? {
                    std::fs::remove_file(real)?;
                } else {
                    Self::metadata_intent(&state, &entry, None).await?;
                }
                epoch.fetch_add(1, Ordering::AcqRel);
                let _ = sender.send(Mutation::Removed(path));
                Ok::<_, Error>(())
            }
            .await;
            match result {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(error)),
            }
        });
    }
    fn rmdir(&mut self, _: &fuser::Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let r = (|| {
            let path = self.child(parent, name)?;
            if self
                .children
                .get(&path)
                .is_some_and(|children| !children.is_empty())
            {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTEMPTY).into());
            }
            let real = local::safe_path(&self.view.root, &path)?;
            std::fs::remove_dir(real)?;
            self.publish_removed(&path);
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn rename(
        &mut self,
        _: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        let result = (|| -> Result<_> {
            let src = self.child(parent, name)?;
            let dst = self.child(newparent, newname)?;
            if flags & !(libc::RENAME_NOREPLACE) != 0 {
                return Err(std::io::Error::from_raw_os_error(libc::EINVAL).into());
            }
            if flags & libc::RENAME_NOREPLACE != 0 && self.view.entries.contains_key(&dst) {
                return Err(std::io::Error::from_raw_os_error(libc::EEXIST).into());
            }
            let e = self.view.target(&src)?.clone();
            if e.folder {
                if self
                    .children
                    .get(&dst)
                    .is_some_and(|children| !children.is_empty())
                {
                    return Err(std::io::Error::from_raw_os_error(libc::ENOTEMPTY).into());
                }
                let prefix = format!("{src}/");
                for (_, child) in self
                    .view
                    .entries
                    .range(prefix.clone()..)
                    .take_while(|(path, _)| path.starts_with(&prefix))
                {
                    if !self.path(child)?.try_exists()? {
                        return Err(Error::Unsupported(
                            "pin the folder before renaming its local tree".into(),
                        ));
                    }
                }
            }
            let from = self.path(&e)?;
            let to = local::safe_path(&self.view.root, &dst)?;
            Ok((src, dst, e, from, to))
        })();
        let (src, dst, entry, from, to) = match result {
            Ok(value) => value,
            Err(error) => {
                reply.error(errno(error));
                return;
            }
        };
        let state = self.state.clone();
        let sender = self.mutation_sender.clone();
        let gate = self.mutation_gate.clone();
        let epoch = self.updates.epoch.clone();
        self.runtime.spawn(async move {
            let _guard = gate.lock().await;
            let result = async {
                let cloud_only = !entry.folder && !from.try_exists()?;
                if !cloud_only {
                    std::fs::create_dir_all(to.parent().unwrap())?;
                }
                // Recheck NOREPLACE after waiting for content/another mutation.
                if flags & libc::RENAME_NOREPLACE != 0 && to.try_exists()? {
                    return Err(std::io::Error::from_raw_os_error(libc::EEXIST).into());
                }
                if cloud_only {
                    Self::metadata_intent(&state, &entry, Some(dst.clone())).await?;
                } else {
                    std::fs::rename(from, to)?;
                }
                epoch.fetch_add(1, Ordering::AcqRel);
                let _ = sender.send(Mutation::Renamed(src, dst));
                Ok::<_, Error>(())
            }
            .await;
            match result {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(error)),
            }
        });
    }
    fn setattr(
        &mut self,
        _: &fuser::Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if uid.is_some_and(|v| v != unsafe { libc::geteuid() })
            || gid.is_some_and(|v| v != unsafe { libc::getegid() })
        {
            reply.error(libc::EPERM);
            return;
        }
        let entry = match self.entry(ino) {
            Ok(entry) => entry,
            Err(error) => {
                reply.error(errno(error));
                return;
            }
        };
        if size.is_none() && atime.is_none() && mtime.is_none() {
            match self.attr(ino, &entry, fh) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(error) => reply.error(errno(error)),
            }
            return;
        }
        let path = match self.path(&entry) {
            Ok(path) => path,
            Err(error) => {
                reply.error(errno(error));
                return;
            }
        };
        let handle = fh.and_then(|h| {
            self.files
                .lock()
                .unwrap()
                .get(&h)
                .map(|f| (f.file.try_clone(), f.writable))
        });
        let state = self.state.clone();
        let gate = self.mutation_gate.clone();
        self.runtime.spawn(async move {
            let _guard = gate.lock().await;
            let result = async {
                let file = if let Some((file, writable)) = handle {
                    if size.is_some() && !writable {
                        return Err(std::io::Error::from_raw_os_error(libc::EBADF).into());
                    }
                    file?
                } else {
                    if entry.folder {
                        OpenOptions::new()
                            .read(true)
                            .custom_flags(libc::O_NOFOLLOW)
                            .open(&path)?
                    } else {
                        let flags = if size.is_some() {
                            libc::O_RDWR
                        } else {
                            libc::O_RDONLY
                        };
                        Self::open_materialized_file(&state, &entry, &path, flags, true).await?
                    }
                };
                if let Some(size) = size {
                    file.set_len(size)?;
                    file.sync_all()?;
                }
                if atime.is_some() || mtime.is_some() {
                    fn ts(value: Option<TimeOrNow>) -> Result<libc::timespec> {
                        Ok(match value {
                            None => libc::timespec {
                                tv_sec: 0,
                                tv_nsec: libc::UTIME_OMIT,
                            },
                            Some(TimeOrNow::Now) => libc::timespec {
                                tv_sec: 0,
                                tv_nsec: libc::UTIME_NOW,
                            },
                            Some(TimeOrNow::SpecificTime(time)) => {
                                let d = time
                                    .duration_since(SystemTime::UNIX_EPOCH)
                                    .map_err(|_| Error::Invalid("timestamp before epoch".into()))?;
                                libc::timespec {
                                    tv_sec: d.as_secs() as i64,
                                    tv_nsec: d.subsec_nanos() as i64,
                                }
                            }
                        })
                    }
                    let times = [ts(atime)?, ts(mtime)?];
                    if unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) } != 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                }
                Ok::<_, Error>(Self::file_attr(ino, &entry, Some(&file.metadata()?)))
            }
            .await;
            match result {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(error) => reply.error(errno(error)),
            }
        });
    }
}

pub fn validate_mountpoint(mount: &Path, backing: &Path, state: &Path) -> Result<PathBuf> {
    let mount = mount.canonicalize()?;
    let backing = backing.canonicalize()?;
    let state = state.canonicalize()?;
    if !mount.is_dir()
        || mount.starts_with(&backing)
        || backing.starts_with(&mount)
        || mount.starts_with(&state)
        || state.starts_with(&mount)
    {
        return Err(Error::Invalid(
            "mount, backing root and state must be separate directories".into(),
        ));
    }
    if std::fs::metadata(&mount)?.uid() != unsafe { libc::geteuid() } {
        return Err(Error::Invalid(
            "mountpoint must belong to current user".into(),
        ));
    }
    if std::fs::read_dir(&mount)?.next().is_some() {
        return Err(Error::Invalid(
            "mountpoint must be empty; no existing files may be hidden".into(),
        ));
    }
    Ok(mount)
}

#[cfg(test)]
mod eviction_lease_tests {
    use super::*;

    #[test]
    fn open_file_lease_blocks_eviction_until_the_last_cloned_handle_closes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.txt");
        std::fs::write(&path, b"keep while open").unwrap();
        for flags in [libc::O_RDONLY, libc::O_RDWR] {
            let file = DriveFs::try_open_materialized_file(&path, flags)
                .unwrap()
                .unwrap();
            let pending_io = file.try_clone().unwrap();
            let eviction = File::open(&path).unwrap();
            assert_eq!(
                fs2::FileExt::try_lock_exclusive(&eviction)
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::WouldBlock
            );
            drop(file);
            assert!(fs2::FileExt::try_lock_exclusive(&eviction).is_err());
            drop(pending_io);
            fs2::FileExt::try_lock_exclusive(&eviction).unwrap();
        }
    }

    #[test]
    fn contended_open_does_not_truncate_before_obtaining_the_file_lease() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.txt");
        std::fs::write(&path, b"preserve these bytes").unwrap();
        let eviction = File::open(&path).unwrap();
        fs2::FileExt::try_lock_exclusive(&eviction).unwrap();
        assert!(
            DriveFs::try_open_materialized_file(&path, libc::O_WRONLY | libc::O_TRUNC)
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"preserve these bytes");
        drop(eviction);
        let file = DriveFs::try_open_materialized_file(&path, libc::O_WRONLY | libc::O_TRUNC)
            .unwrap()
            .unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
    }

    #[test]
    fn pending_open_rejects_a_quarantined_replaced_or_unlinked_inode() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.txt");
        std::fs::write(&path, b"original").unwrap();
        let pending = File::open(&path).unwrap();
        std::fs::rename(&path, temp.path().join("quarantine")).unwrap();
        assert!(DriveFs::protect_materialized_file(&path, pending)
            .unwrap()
            .is_none());

        std::fs::write(&path, b"replacement").unwrap();
        let old = File::open(temp.path().join("quarantine")).unwrap();
        assert!(DriveFs::protect_materialized_file(&path, old)
            .unwrap()
            .is_none());
        let removed = File::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(DriveFs::protect_materialized_file(&path, removed)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn pending_open_retries_after_a_short_eviction_lease() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.txt");
        std::fs::write(&path, b"available again").unwrap();
        let eviction = File::open(&path).unwrap();
        fs2::FileExt::try_lock_exclusive(&eviction).unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            drop(eviction);
        });
        let entry = Entry {
            path: "file.txt".into(),
            key: "r:file".into(),
            folder: false,
            size: 15,
            file_id: None,
            unsupported: false,
        };
        let file = tokio::time::timeout(
            Duration::from_secs(2),
            DriveFs::open_materialized_file(temp.path(), &entry, &path, libc::O_RDONLY, false),
        )
        .await
        .unwrap()
        .unwrap();
        let mut bytes = [0; 15];
        assert_eq!(file.read_at(&mut bytes, 0).unwrap(), bytes.len());
        assert_eq!(&bytes, b"available again");
        release.await.unwrap();
    }
}

#[cfg(test)]
mod responsiveness_tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, DriveFs) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("local");
        let state = temp.path().join("state");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(root.join("local.txt"), b"cached").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = Database::open(&state.join("state.sqlite3"), "test")
                .await
                .unwrap();
            for (key, value) in [("root", root.to_str().unwrap()), ("folder", "root")] {
                sqlx::query("INSERT OR REPLACE INTO settings(key,value) VALUES (?,?)")
                    .bind(key)
                    .bind(value)
                    .execute(&db.pool)
                    .await
                    .unwrap();
            }
            db.pool.close().await;
        });
        let fs = DriveFs::new(&state, "test").unwrap();
        (temp, fs)
    }

    #[test]
    fn cached_metadata_does_not_wait_for_a_busy_database_connection() {
        let (_temp, fs) = fixture();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool = fs.db.pool.clone();
        let connection = rt.block_on(pool.acquire()).unwrap();
        // The former refresh path synchronously acquired SQLite after one second.
        std::thread::sleep(Duration::from_millis(1100));
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut fs = fs;
            let root = fs.entry(1).unwrap();
            assert!(root.folder);
            assert_eq!(fs.children[""], vec!["local.txt"]);
            sender.send(()).unwrap();
        });
        let result = receiver.recv_timeout(Duration::from_millis(500));
        rt.block_on(async {
            drop(connection);
        });
        worker.join().unwrap();
        assert!(result.is_ok(), "metadata callback blocked on SQLite");
    }

    #[test]
    fn namespace_snapshots_share_storage_but_local_mutations_are_isolated() {
        let (_temp, mut fs) = fixture();
        let snapshot = fs.view.clone();
        assert!(Arc::ptr_eq(&snapshot.entries, &fs.view.entries));
        fs.publish_created("new-local.txt", false);
        assert!(!Arc::ptr_eq(&snapshot.entries, &fs.view.entries));
        assert!(!snapshot.entries.contains_key("new-local.txt"));
        assert!(fs.view.entries.contains_key("new-local.txt"));
        fs.publish_removed("local.txt");
        assert!(snapshot.entries.contains_key("local.txt"));
        assert!(!fs.view.entries.contains_key("local.txt"));
    }

    #[test]
    fn local_namespace_updates_preserve_renamed_tree_inodes_without_database_reload() {
        let (_temp, mut fs) = fixture();
        fs.publish_created("folder", true);
        fs.publish_created("folder/nested", true);
        fs.publish_created("folder/nested/file.txt", false);
        let original = fs.inodes["p:folder/nested/file.txt"];
        fs.publish_rename("folder", "renamed");
        assert!(!fs.view.entries.contains_key("folder"));
        assert_eq!(fs.inodes["p:renamed/nested/file.txt"], original);
        assert_eq!(fs.children["renamed"], vec!["renamed/nested"]);
        assert_eq!(
            fs.children["renamed/nested"],
            vec!["renamed/nested/file.txt"]
        );
        fs.publish_created("folder", true);
        fs.publish_created("folder/nested", true);
        fs.publish_created("folder/nested/file.txt", false);
        assert_ne!(fs.inodes["p:folder/nested/file.txt"], original);
        fs.publish_removed("folder/nested/file.txt");
        assert!(fs.children["folder/nested"].is_empty());
        assert!(fs.view.entries.contains_key("renamed/nested/file.txt"));
    }

    #[test]
    fn atomic_editor_replace_preserves_source_inode_and_removes_target_mapping() {
        let (_temp, mut fs) = fixture();
        fs.publish_created("save.tmp", false);
        let source_inode = fs.inodes["p:save.tmp"];
        let previous_inode = fs.inodes["p:local.txt"];
        fs.publish_rename("save.tmp", "local.txt");
        assert_eq!(fs.inodes["p:local.txt"], source_inode);
        assert!(!fs.nodes.contains_key(&previous_inode));
        assert!(!fs.view.entries.contains_key("save.tmp"));
        assert_eq!(fs.children[""], vec!["local.txt"]);
    }

    #[test]
    fn office_working_files_remain_usable_in_the_mount_and_promote_to_documents() {
        let (_temp, mut fs) = fixture();
        fs.publish_created("~WRD1234.tmp", false);
        fs.publish_created("~$report.docx", false);
        let working_inode = fs.inodes["p:~WRD1234.tmp"];
        assert!(fs.entry(working_inode).is_ok());
        assert!(fs.view.target("~$report.docx").is_ok());
        fs.publish_rename("~WRD1234.tmp", "report.docx");
        assert_eq!(fs.inodes["p:report.docx"], working_inode);
        assert!(fs.view.target("~WRD1234.tmp").is_err());
        assert!(fs.view.target("report.docx").is_ok());
        fs.publish_removed("~$report.docx");
        assert!(fs.view.target("~$report.docx").is_err());
    }

    #[test]
    fn stale_background_snapshot_cannot_undo_a_local_mutation() {
        let (_temp, mut fs) = fixture();
        let (sender, receiver) = mpsc::sync_channel(1);
        fs.updates = NamespaceUpdates {
            receiver,
            epoch: Arc::new(AtomicU64::new(1)),
            stop: Arc::new(AtomicBool::new(false)),
            pending: Arc::new(AtomicBool::new(true)),
            badges: Arc::new(RwLock::new(Arc::new(HashMap::new()))),
        };
        sender
            .send((
                0,
                View {
                    root: PathBuf::from("/stale"),
                    entries: Default::default(),
                    folder_badges: Default::default(),
                },
            ))
            .unwrap_or_else(|_| panic!("mailbox closed"));
        fs.refresh(false).unwrap();
        assert!(fs.view.entries.contains_key("local.txt"));
        assert_ne!(fs.backing_root(), Path::new("/stale"));
    }
}
