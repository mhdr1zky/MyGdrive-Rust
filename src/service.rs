//! Single-writer service and versioned, same-user Unix IPC.
use crate::{
    connectivity::ConnectionState, engine::SyncProgress, local::LocalWatcher, now_ms, Error,
    Result, SyncEngine, SyncReport,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, Notify, Semaphore},
    task::JoinSet,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
const VERSION: u32 = 1;
const MAX_MESSAGE: u64 = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub version: u32,
    pub account: String,
    pub root: PathBuf,
    pub drive_folder: String,
    pub interval_seconds: u64,
    pub cache_max_bytes: Option<u64>,
}
impl ServiceConfig {
    pub fn validate(&self) -> Result<()> {
        if self.version != VERSION
            || self.account.is_empty()
            || self.drive_folder.is_empty()
            || !(1..=86400).contains(&self.interval_seconds)
            || !self.root.is_absolute()
        {
            return Err(Error::Invalid(
                "invalid service configuration/version".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub version: u32,
    pub session_id: String,
    pub pid: u32,
    pub running: bool,
    pub started_at_ms: i64,
    pub heartbeat_at_ms: i64,
    pub stopped_at_ms: Option<i64>,
    pub config: ServiceConfig,
    pub phase: String,
    pub connection: ConnectionState,
    pub paused: bool,
    pub cycle_active: bool,
    pub sync_requested: bool,
    pub shutdown_requested: bool,
    pub local_watcher_active: bool,
    pub last_sync_started_ms: Option<i64>,
    pub last_sync_finished_ms: Option<i64>,
    pub last_success_ms: Option<i64>,
    pub cycles_finished: u64,
    pub pending_operations: i64,
    pub pending_files: i64,
    pub progress: SyncProgress,
    pub last_report: Option<SyncReport>,
    pub last_error: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Status,
    Pause,
    Resume,
    SyncNow,
    Shutdown,
    OfflineList,
    OfflineSet,
    Hydrate,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub request_id: String,
    pub session_id: Option<String>,
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline: Option<crate::offline::Request>,
}
impl Request {
    pub fn new(action: Action) -> Self {
        Self {
            version: VERSION,
            request_id: uuid::Uuid::new_v4().to_string(),
            session_id: None,
            action,
            offline: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub version: u32,
    pub request_id: String,
    pub accepted: bool,
    pub duplicate: bool,
    pub error: Option<String>,
    pub status: ServiceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline: Option<crate::offline::Page>,
}
struct Control {
    status: ServiceStatus,
    recent: VecDeque<(String, Action)>,
    local_wake: u64,
}
impl Control {
    fn apply(&mut self, request: &Request) -> Reply {
        let mut duplicate = false;
        let error =
            if request.version != VERSION || uuid::Uuid::parse_str(&request.request_id).is_err() {
                Some("unsupported protocol version or invalid request ID")
            } else if request.offline.is_some()
                || matches!(
                    request.action,
                    Action::OfflineList | Action::OfflineSet | Action::Hydrate
                )
            {
                Some("unsupported payload for service control")
            } else if request
                .session_id
                .as_ref()
                .is_some_and(|s| s != &self.status.session_id)
            {
                Some("service session changed; fetch status before retrying")
            } else if let Some((_, action)) =
                self.recent.iter().find(|(id, _)| id == &request.request_id)
            {
                duplicate = true;
                if action != &request.action {
                    Some("request ID already used for another action")
                } else {
                    None
                }
            } else if request.action == Action::SyncNow && self.status.paused {
                Some("service is paused; resume first")
            } else if self.status.shutdown_requested && request.action != Action::Status {
                Some("service is shutting down")
            } else {
                None
            };
        if error.is_none() && !duplicate && request.action != Action::Status {
            match request.action {
                Action::Pause => {
                    self.status.paused = true;
                    self.status.sync_requested = false;
                    self.status.phase = if self.status.cycle_active {
                        "pausing"
                    } else {
                        "paused"
                    }
                    .into();
                }
                Action::Resume => {
                    if self.status.paused {
                        self.status.paused = false;
                        self.status.sync_requested = true;
                        self.status.phase = if self.status.cycle_active {
                            "syncing"
                        } else {
                            "idle"
                        }
                        .into();
                    }
                }
                Action::SyncNow => {
                    if !self.status.paused {
                        self.status.sync_requested = true;
                    }
                }
                Action::Shutdown => {
                    self.status.shutdown_requested = true;
                    self.status.phase = "stopping".into();
                }
                Action::Status => {}
                Action::OfflineList | Action::OfflineSet | Action::Hydrate => unreachable!(),
            }
            if self.recent.len() == 256 {
                self.recent.pop_front();
            }
            self.recent
                .push_back((request.request_id.clone(), request.action));
        }

        Reply {
            version: VERSION,
            request_id: request.request_id.clone(),
            accepted: error.is_none(),
            duplicate,
            error: error.map(String::from),
            status: self.status.clone(),
            offline: None,
        }
    }
}

pub fn config_path(state: &Path) -> PathBuf {
    state.join("service-config.json")
}
pub fn status_path(state: &Path) -> PathBuf {
    state.join("service-status.json")
}
pub fn socket_path(state: &Path) -> PathBuf {
    state.join("control/service.sock")
}
pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("state path missing parent".into()))?;
    let temp = parent.join(format!(".service-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        f.write_all(&serde_json::to_vec_pretty(value)?)?;
        f.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
pub fn load_config(state: &Path) -> Result<ServiceConfig> {
    let config: ServiceConfig = read_json(&config_path(state))?;
    config.validate()?;
    Ok(config)
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > 128 * 1024 {
        return Err(Error::Invalid("invalid service state file".into()));
    }
    Ok(serde_json::from_reader(std::io::Read::take(
        file,
        128 * 1024 + 1,
    ))?)
}
pub fn last_status(state: &Path) -> Result<ServiceStatus> {
    let mut status: ServiceStatus = read_json(&status_path(state))?;
    // A disk snapshot is historical. It must never attest to a live process.
    status.running = false;
    status.local_watcher_active = false;
    status.phase = "last_known".into();
    Ok(status)
}
struct SocketFile {
    path: PathBuf,
    inode: u64,
    _lock: File,
}
impl Drop for SocketFile {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.ino() == self.inode && m.file_type().is_socket())
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
fn bind_socket(state: &Path) -> Result<(UnixListener, SocketFile)> {
    let path = socket_path(state);
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let meta = fs::symlink_metadata(parent)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(Error::Invalid("unsafe control directory".into()));
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(parent.join("service.lock"))?;
    lock.try_lock_exclusive()
        .map_err(|_| Error::Invalid("another control server is running".into()))?;
    match fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_socket() && m.uid() == unsafe { libc::geteuid() } => {
            fs::remove_file(&path)?
        }
        Ok(_) => {
            return Err(Error::Invalid(
                "refusing to replace non-socket control path".into(),
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    // Caller already owns SyncEngine's exclusive state lock.
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let guard = SocketFile {
        inode: fs::symlink_metadata(&path)?.ino(),
        path,
        _lock: lock,
    };
    Ok((listener, guard))
}
async fn frame(reader: &mut (impl tokio::io::AsyncBufRead + Unpin), limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_until(b'\n', &mut bytes).await?;
    if bytes.len() as u64 > limit || bytes.last() != Some(&b'\n') {
        return Err(Error::Invalid("IPC frame too large or incomplete".into()));
    }
    Ok(bytes)
}
async fn connection(
    mut stream: UnixStream,
    shared: Arc<Mutex<Control>>,
    wake: Arc<Notify>,
    state: PathBuf,
    engine: Arc<SyncEngine>,
    observer: crate::db::Database,
) -> Result<()> {
    if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
        return Err(Error::Invalid("IPC peer user rejected".into()));
    }
    let (read, mut write) = stream.split();
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        frame(&mut BufReader::new(read), MAX_MESSAGE),
    )
    .await
    .map_err(|_| Error::Invalid("IPC request timed out".into()))??;
    let request: Request =
        serde_json::from_slice(&bytes).map_err(|_| Error::Invalid("invalid IPC request".into()))?;
    let reply = if matches!(
        request.action,
        Action::OfflineList | Action::OfflineSet | Action::Hydrate
    ) {
        offline_request(&request, &engine, &shared, &wake).await
    } else {
        // Read the command ledger through a separate WAL reader. Never await
        // the engine's single writer connection while holding the control lock.
        let used = if request.action == Action::Status {
            0
        } else {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM offline_commands WHERE request_id=?")
                .bind(&request.request_id)
                .fetch_one(&observer.pool)
                .await?
        };
        let mut c = shared.lock().await;
        c.status.heartbeat_at_ms = now_ms();
        let reply = if used > 0 {
            Reply {
                version: VERSION,
                request_id: request.request_id.clone(),
                accepted: false,
                duplicate: false,
                error: Some("request ID already used for offline selection".into()),
                status: c.status.clone(),
                offline: None,
            }
        } else {
            c.apply(&request)
        };
        if reply.accepted && request.action != Action::Status {
            atomic_json(&status_path(&state), &c.status)?;
        }
        reply
    };
    // Apply before ACK: retry with the same ID never schedules a second action.
    wake.notify_one();
    let mut data = serde_json::to_vec(&reply)?;
    data.push(b'\n');
    write.write_all(&data).await?;
    Ok(())
}
async fn offline_request(
    request: &Request,
    engine: &SyncEngine,
    shared: &Mutex<Control>,
    wake: &Notify,
) -> Reply {
    let valid = {
        let c = shared.lock().await;
        request.version == VERSION
            && uuid::Uuid::parse_str(&request.request_id).is_ok()
            && request
                .session_id
                .as_ref()
                .is_none_or(|id| id == &c.status.session_id)
            && (!matches!(request.action, Action::OfflineSet | Action::Hydrate)
                || request.session_id.is_some())
            && !c.status.shutdown_requested
            && !c.recent.iter().any(|(id, _)| id == &request.request_id)
    };
    let result = async {
        if !valid {
            return Err(Error::Invalid("invalid offline request/session".into()));
        }
        match (&request.action, &request.offline) {
            (Action::OfflineList, Some(crate::offline::Request::List { parent, offset })) => {
                Ok((Some(engine.offline_page(parent, *offset).await?), false))
            }
            (Action::OfflineSet, Some(crate::offline::Request::Set { target, pinned })) => {
                let duplicate = engine
                    .offline_select(&request.request_id, target, *pinned)
                    .await?;
                let mut c = shared.lock().await;
                if !c.status.paused && !c.status.shutdown_requested {
                    c.status.sync_requested = true;
                    wake.notify_one();
                }
                Ok((None, duplicate))
            }
            (
                Action::OfflineSet,
                Some(crate::offline::Request::Metadata {
                    file_id,
                    source,
                    destination,
                }),
            ) => {
                let duplicate = engine
                    .virtual_metadata(&request.request_id, file_id, source, destination.as_deref())
                    .await?;
                let mut c = shared.lock().await;
                if !c.status.paused && !c.status.shutdown_requested {
                    c.status.sync_requested = true;
                    wake.notify_one();
                }
                Ok((None, duplicate))
            }
            (Action::Hydrate, Some(crate::offline::Request::Hydrate { file_id })) => {
                if shared.lock().await.status.paused {
                    return Err(Error::Invalid("service paused".into()));
                }
                engine.hydrate(file_id).await?;
                Ok((None, false))
            }
            _ => Err(Error::Invalid("offline payload mismatch".into())),
        }
    }
    .await;
    let (page, duplicate, error) = match result {
        Ok((p, d)) => (p, d, None),
        Err(_) => (
            None,
            false,
            Some("offline request failed; refresh status and folder before retrying".to_string()),
        ),
    };
    Reply {
        version: VERSION,
        request_id: request.request_id.clone(),
        accepted: error.is_none(),
        duplicate,
        error,
        status: shared.lock().await.status.clone(),
        offline: page,
    }
}
pub async fn send(state: &Path, request: &Request) -> Result<Reply> {
    let deadline = if request.action == Action::Hydrate {
        600
    } else if matches!(request.action, Action::OfflineList | Action::OfflineSet) {
        30
    } else {
        5
    };
    tokio::time::timeout(Duration::from_secs(deadline), async {
        let mut socket = UnixStream::connect(socket_path(state)).await?;
        if socket.peer_cred()?.uid() != unsafe { libc::geteuid() } {
            return Err(Error::Invalid("IPC server user rejected".into()));
        }
        let mut bytes = serde_json::to_vec(request)?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_MESSAGE {
            return Err(Error::Invalid("IPC request too large".into()));
        }
        socket.write_all(&bytes).await?;
        let reply: Reply =
            serde_json::from_slice(&frame(&mut BufReader::new(socket), 128 * 1024).await?)?;
        if reply.version != VERSION || reply.request_id != request.request_id {
            return Err(Error::Invalid("IPC response mismatch".into()));
        }
        Ok(reply)
    })
    .await
    .map_err(|_| Error::Invalid("service control timed out".into()))?
}
fn error_kind(error: &Error) -> String {
    serde_json::to_value(ConnectionState::from_error(error))
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "service_error".into())
}
async fn schedule(
    engine: Arc<SyncEngine>,
    shared: Arc<Mutex<Control>>,
    wake: Arc<Notify>,
    config: ServiceConfig,
) -> Result<()> {
    let mut due = Instant::now();
    let mut failures = 0u32;
    let mut local_seen = 0;
    loop {
        let (local_ticket, check_local) = {
            let c = shared.lock().await;
            (
                c.local_wake,
                !c.status.paused && !c.status.shutdown_requested && c.local_wake != local_seen,
            )
        };
        // A cycle may already have consumed the generation announced by a late
        // watcher event. Validate remaining dirty work instead of running again.
        let local_pending = check_local
            && LocalWatcher::pending_changes(&engine.scanner)
                .await
                .map(|files| !files.is_empty())
                .unwrap_or(true);
        if check_local {
            local_seen = local_ticket;
        }
        let start = {
            let mut c = shared.lock().await;
            if c.status.shutdown_requested {
                return Ok(());
            }
            if !c.status.paused
                && (c.status.sync_requested || local_pending || Instant::now() >= due)
            {
                c.status.sync_requested = false;
                c.status.cycle_active = true;
                c.status.phase = "syncing".into();
                c.status.connection = ConnectionState::Connecting;
                c.status.last_sync_started_ms = Some(now_ms());
                true
            } else {
                false
            }
        };
        if start {
            let result = engine.sync_once().await;
            let mut c = shared.lock().await;
            c.status.cycle_active = false;
            c.status.last_sync_finished_ms = Some(now_ms());
            c.status.cycles_finished += 1;
            let delay = match result {
                Ok(report) => {
                    failures = 0;
                    c.status.connection = ConnectionState::Online;
                    c.status.last_error = if report.errors.is_empty() {
                        None
                    } else {
                        Some("one or more file operations failed; review file status".into())
                    };
                    if report.errors.is_empty() {
                        c.status.last_success_ms = Some(now_ms());
                    }
                    // Reports may include user paths; keep the IPC/disk diagnostic bounded and generic.
                    c.status.last_report = Some(SyncReport {
                        errors: report
                            .errors
                            .iter()
                            .take(32)
                            .map(|_| "file operation failed".into())
                            .collect(),
                        ..report
                    });
                    config.interval_seconds
                }
                Err(e) => {
                    failures = failures.saturating_add(1);
                    c.status.connection = ConnectionState::from_error(&e);
                    c.status.last_error = Some(error_kind(&e));
                    if matches!(e, Error::AuthRequired) {
                        config.interval_seconds.max(60)
                    } else {
                        config
                            .interval_seconds
                            .saturating_mul(1u64 << failures.min(5))
                            .min(300)
                    }
                }
            };
            c.status.phase = if c.status.shutdown_requested {
                "stopping"
            } else if c.status.paused {
                "paused"
            } else {
                "idle"
            }
            .into();
            let prune = config
                .cache_max_bytes
                .filter(|_| !c.status.paused && !c.status.shutdown_requested);
            drop(c);
            if let Some(budget) = prune {
                if engine.prune_cache(budget).await.is_err() {
                    shared.lock().await.status.last_error = Some("cache pruning deferred".into());
                }
            }
            // Large inventory snapshots are gone here. Give their unused heap
            // pages back outside filesystem/DB locks, with a process-wide cap.
            let _ = tokio::task::spawn_blocking(crate::memory::release_unused).await;
            due = Instant::now() + Duration::from_secs(delay);
            continue;
        }
        let paused = shared.lock().await.status.paused;
        if paused {
            wake.notified().await;
        } else {
            tokio::select! {_ = wake.notified()=>{}, _ = tokio::time::sleep_until(due)=>{}}
        }
    }
}

/// Runs in foreground; IPC shutdown and signals drain the active cycle before exit.
pub async fn run(
    engine: Arc<SyncEngine>,
    state: &Path,
    config: ServiceConfig,
    start_paused: bool,
    stop: CancellationToken,
) -> Result<()> {
    config.validate()?;
    if config.root != engine.scanner.root
        || config.account != engine.db.account
        || engine.db.setting("folder").await?.as_deref() != Some(config.drive_folder.as_str())
    {
        return Err(Error::Invalid(
            "service config differs from engine binding".into(),
        ));
    }
    let (listener, _socket) = bind_socket(state)?;
    atomic_json(&config_path(state), &config)?;
    let start_paused = match read_json::<ServiceStatus>(&status_path(state)) {
        Ok(old)
            if old.config.root == config.root
                && old.config.account == config.account
                && old.config.drive_folder == config.drive_folder =>
        {
            start_paused || old.paused
        }
        Ok(_) => start_paused,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => start_paused,
        Err(_) => true, // Unknown persisted pause intent: require explicit resume.
    };
    let started = now_ms();
    let shared = Arc::new(Mutex::new(Control {
        status: ServiceStatus {
            version: VERSION,
            session_id: uuid::Uuid::new_v4().to_string(),
            pid: std::process::id(),
            running: true,
            started_at_ms: started,
            heartbeat_at_ms: started,
            stopped_at_ms: None,
            config: config.clone(),
            phase: if start_paused { "paused" } else { "starting" }.into(),
            connection: ConnectionState::Unknown,
            paused: start_paused,
            cycle_active: false,
            sync_requested: !start_paused,
            shutdown_requested: false,
            local_watcher_active: true,
            last_sync_started_ms: None,
            last_sync_finished_ms: None,
            last_success_ms: None,
            cycles_finished: 0,
            pending_operations: 0,
            pending_files: 0,
            progress: SyncProgress::default(),
            last_report: None,
            last_error: None,
        },
        recent: VecDeque::new(),
        local_wake: 0,
    }));
    engine.recover_moves().await?;
    engine.recover_displaced().await?;
    let observer =
        crate::db::Database::read_only(&state.join("state.sqlite3"), &engine.db.account).await?;
    let watcher = LocalWatcher::start(&engine.scanner.root)?;
    let watcher_stop = CancellationToken::new();
    let scanner = engine.scanner.clone();
    let worker_stop = watcher_stop.clone();
    let (local_changes, mut local_events) = tokio::sync::mpsc::channel(1);
    let mut worker = tokio::spawn(async move {
        watcher
            .run_notifying(scanner, worker_stop, local_changes)
            .await
    });
    let wake = Arc::new(Notify::new());
    let mut scheduler = tokio::spawn(schedule(
        engine.clone(),
        shared.clone(),
        wake.clone(),
        config,
    ));
    let permits = Arc::new(Semaphore::new(16));
    let mut clients = JoinSet::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(2));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut worker_result = None;
    let mut scheduler_result = None;
    let mut failure = None;
    loop {
        tokio::select! {
            Some(()) = local_events.recv() => {
                let mut c = shared.lock().await;
                if !c.status.paused && !c.status.shutdown_requested {
                    c.local_wake = c.local_wake.wrapping_add(1);
                    wake.notify_one();
                }
            },
            _ = stop.cancelled(), if !shared.lock().await.status.shutdown_requested => { let mut c=shared.lock().await; c.status.shutdown_requested=true; c.status.phase="stopping".into(); wake.notify_one(); },
            result = &mut scheduler => {scheduler_result=Some(result); break;},
            result = &mut worker => {worker_result=Some(result); shared.lock().await.status.last_error=Some("local watcher stopped".into()); failure=Some(Error::Invalid("local watcher stopped".into())); break;},
            Some(_) = clients.join_next(), if !clients.is_empty()=>{},
            result = listener.accept() => match result {
                Ok((stream,_)) => if let Ok(permit)=permits.clone().try_acquire_owned() { let c=shared.clone();let w=wake.clone(); let client_state=state.to_path_buf(); let client_engine=engine.clone(); let client_observer=observer.clone(); clients.spawn(async move {let _permit=permit;let _=tokio::time::timeout(Duration::from_secs(600),connection(stream,c,w,client_state,client_engine,client_observer)).await;}); },
                Err(e)=>{failure=Some(e.into());break;}
            },
            _ = heartbeat.tick() => {
                let pending_ops=sqlx::query_scalar::<_,i64>("SELECT count(*) FROM sync_operations WHERE stage NOT IN ('done','cancelled')").fetch_one(&observer.pool).await;
                let pending_files=sqlx::query_scalar::<_,i64>("SELECT count(*) FROM files WHERE sync_status NOT IN ('synced','online_only','tombstone')").fetch_one(&observer.pool).await;
                let snapshot = {
                    let mut c = shared.lock().await;
                    c.status.heartbeat_at_ms = now_ms();
                    c.status.progress = engine.progress();
                    if !c.status.cycle_active {
                        c.status.progress.stage = c.status.phase.clone();
                    }
                    if let Ok(n) = pending_ops { c.status.pending_operations = n; }
                    if let Ok(n) = pending_files { c.status.pending_files = n; }
                    atomic_json(&status_path(state), &c.status)
                };
                if let Err(e)=snapshot{failure=Some(e);break;}
            }
        }
    }
    {
        let mut c = shared.lock().await;
        c.status.shutdown_requested = true;
        c.status.phase = "stopping".into();
    }
    wake.notify_one();
    if scheduler_result.is_none() {
        scheduler_result = Some(scheduler.await);
    }
    watcher_stop.cancel();
    if worker_result.is_none() {
        worker_result = Some(worker.await);
    }
    while clients.join_next().await.is_some() {}
    let snapshot = {
        let mut c = shared.lock().await;
        c.status.running = false;
        c.status.local_watcher_active = false;
        c.status.phase = "stopped".into();
        c.status.heartbeat_at_ms = now_ms();
        c.status.stopped_at_ms = Some(now_ms());
        c.status.clone()
    };
    atomic_json(&status_path(state), &snapshot)?;
    for result in [scheduler_result, worker_result].into_iter().flatten() {
        result.map_err(|_| Error::Invalid("service worker failed".into()))??;
    }
    if let Some(e) = failure {
        return Err(e);
    }
    Ok(())
}

pub async fn run_owned(
    engine: Arc<SyncEngine>,
    state: PathBuf,
    config: ServiceConfig,
    paused: bool,
    stop: CancellationToken,
) -> Result<()> {
    run(engine, &state, config, paused, stop).await
}
