//! GTK-independent desktop controller. All filesystem/IPC work stays off the UI thread.
use crate::{
    service::{self, Action, Request, ServiceStatus},
    Error, Result,
};
use std::{
    fs::OpenOptions,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub live: bool,
    pub status: Option<ServiceStatus>,
    pub config: Option<service::ServiceConfig>,
    pub account: Option<crate::oauth::AccountIdentity>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    Reconnecting,
    Offline,
    AuthRequired,
    Throttled,
    Error,
    Stopped,
}
impl Notice {
    pub fn title(self) -> &'static str {
        match self {
            Self::Reconnecting => "Menghubungkan ulang ke layanan",
            Self::Offline => "Offline — menunggu koneksi",
            Self::AuthRequired => "Login Google diperlukan",
            Self::Throttled => "Menunggu batas layanan Drive",
            Self::Error => "Sinkronisasi perlu diperiksa",
            Self::Stopped => "Layanan tidak aktif / sedang berhenti",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::Reconnecting=>"Antarmuka belum terhubung ke layanan. Riwayat selesai tetap tersimpan; status sedang diperiksa ulang.",
            Self::Offline=>"Koneksi Drive terputus. Perubahan tetap tersimpan dan akan dicoba ulang otomatis.",
            Self::AuthRequired=>"Izin Google perlu diperbarui. Buka Akun dan folder → Login ulang akun terpilih.",
            Self::Throttled=>"Drive membatasi permintaan. Aplikasi menunggu sebelum mencoba lagi.",
            Self::Error=>"Layanan melaporkan kesalahan sinkronisasi. Riwayat unggahan selesai di bawah bukan status seluruh layanan; periksa operasi tertunda dan dashboard.",
            Self::Stopped=>"Layanan tidak aktif atau sedang berhenti. Riwayat di bawah adalah hasil operasi sebelumnya.",
        }
    }
    pub fn is_error(self) -> bool {
        matches!(self, Self::AuthRequired | Self::Error)
    }
}
impl Snapshot {
    /// One policy for tray colors and popup explanations; history alone is not health.
    pub fn notice(&self) -> Option<Notice> {
        use crate::connectivity::ConnectionState;
        let Some(status) = self.status.as_ref().filter(|_| self.live) else {
            return Some(Notice::Reconnecting);
        };
        if !status.running || status.shutdown_requested {
            return Some(Notice::Stopped);
        }
        match status.connection {
            ConnectionState::Offline => return Some(Notice::Offline),
            ConnectionState::AuthRequired => return Some(Notice::AuthRequired),
            ConnectionState::Throttled => return Some(Notice::Throttled),
            ConnectionState::ServiceError => return Some(Notice::Error),
            _ => {}
        }
        match status.last_error.as_deref() {
            Some("offline") => Some(Notice::Offline),
            Some("auth_required") => Some(Notice::AuthRequired),
            Some("throttled") => Some(Notice::Throttled),
            Some(_) => Some(Notice::Error),
            None => None,
        }
    }
    pub fn configuration(&self) -> Option<&service::ServiceConfig> {
        self.status
            .as_ref()
            .map(|s| &s.config)
            .or(self.config.as_ref())
    }
    pub fn title(&self) -> &'static str {
        let Some(s) = self.status.as_ref().filter(|_| self.live) else {
            return "Service tidak aktif";
        };
        if s.shutdown_requested {
            "Sedang berhenti"
        } else if s.paused && s.cycle_active {
            "Menunggu jeda"
        } else if s.paused {
            "Sinkronisasi dijeda"
        } else if s.cycle_active && s.progress.has_file_activity(crate::now_ms()) {
            "Menyinkronkan"
        } else if s.cycle_active {
            "Memeriksa perubahan Drive"
        } else {
            match s.connection {
                crate::connectivity::ConnectionState::AuthRequired => "Login diperlukan",
                crate::connectivity::ConnectionState::Offline => "Offline",
                crate::connectivity::ConnectionState::Throttled => "Menunggu batas layanan",
                crate::connectivity::ConnectionState::ServiceError => "Perlu diperiksa",
                _ if s.last_error.is_some() => "Perlu diperiksa",
                _ if s.last_success_ms.is_some() && s.pending_files == 0 => "Semua tersinkron",
                _ => "Menunggu sinkronisasi",
            }
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub enum Command {
    Pause,
    Resume,
    SyncNow,
    Shutdown,
    ShutdownAndWait,
    Start,
    SetAutostart(bool),
}
#[derive(Debug)]
pub enum Event {
    Snapshot(Box<Snapshot>),
    Completed(std::result::Result<&'static str, String>),
    Autostart(std::result::Result<bool, String>),
    Activity(ActivityPage),
    ExitReady,
}
pub struct Controller {
    pub commands: tokio::sync::mpsc::Sender<Command>,
    pub events: mpsc::Receiver<Event>,
    stop: CancellationToken,
}
impl Controller {
    pub fn stop(&self) {
        self.stop.cancel();
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
pub fn spawn(state: PathBuf, binary: PathBuf) -> Result<Controller> {
    spawn_observer(state, binary, true)
}
pub fn spawn_observer(
    state: PathBuf,
    binary: PathBuf,
    refresh_account: bool,
) -> Result<Controller> {
    let (commands, mut input) = tokio::sync::mpsc::channel(8);
    let (events, receiver) = mpsc::channel();
    let stop = CancellationToken::new();
    let worker_stop = stop.clone();
    std::thread::Builder::new().name("mygdrive-desktop-io".into()).spawn(move || {
        let runtime=match tokio::runtime::Builder::new_current_thread().enable_all().build(){Ok(r)=>r,Err(_)=>{let _=events.send(Event::Completed(Err("Worker aplikasi tidak dapat dimulai.".into())));return;}};
        runtime.block_on(async move {
            let settings=||crate::autostart::for_profile(&state,&binary).and_then(|s|s.enabled()).map_err(|_|"Status autostart belum dapat dibaca.".to_string());
            let _=events.send(Event::Autostart(settings()));
            let _=events.send(Event::Snapshot(Box::new(read_snapshot(&state).await)));
            if let Ok(config)=service::load_config(&state) {
                let token=crate::oauth::token_path(&state,&config.account);
                if refresh_account && matches!(crate::oauth::saved_identity(&token),Ok(None)) {
                    let _=tokio::time::timeout(Duration::from_secs(8),crate::oauth::refresh_identity(&token)).await;
                }
            }
            let mut poll=tokio::time::interval(Duration::from_secs(2));poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let work=async {
                    tokio::select!{
                        command=input.recv()=>{
                            let Some(command)=command else{return false;};
                            let result=perform(&state,&binary,command).await;
                            if matches!(command,Command::SetAutostart(_)){let _=events.send(Event::Autostart(settings()));}
                            let event=if matches!(command,Command::ShutdownAndWait) && result.is_ok(){Event::ExitReady}else{Event::Completed(result.map_err(|_|"Perintah belum terkonfirmasi. Periksa status service sebelum mencoba kembali.".into()))};
                            if events.send(event).is_err(){return false;}
                        },
                        _=poll.tick()=>{},
                    }
                    let snapshot=read_snapshot(&state).await;
                    if events.send(Event::Snapshot(Box::new(snapshot))).is_err() { return false; }
                    if let Ok(items)=read_activity_page(&state).await { let _=events.send(Event::Activity(items)); }
                    true
                };
                tokio::select!{_ = worker_stop.cancelled()=>break, keep=work=>if !keep{break;}}
            }
        });
    })?;
    Ok(Controller {
        commands,
        events: receiver,
        stop,
    })
}
pub async fn read_snapshot(state: &Path) -> Snapshot {
    let config = service::load_config(state).ok();
    let account = config.as_ref().and_then(|c| {
        crate::oauth::saved_identity(&crate::oauth::token_path(state, &c.account))
            .ok()
            .flatten()
    });
    match service::send(state, &Request::new(Action::Status)).await {
        Ok(reply) if reply.accepted => Snapshot {
            live: true,
            status: Some(reply.status),
            config: None,
            account,
        },
        _ => Snapshot {
            live: false,
            status: service::last_status(state).ok(),
            config,
            account,
        },
    }
}
pub async fn perform(state: &Path, binary: &Path, command: Command) -> Result<&'static str> {
    if matches!(command, Command::ShutdownAndWait) && engine_available(state)? {
        return Ok("Service sudah berhenti.");
    }
    if let Command::SetAutostart(enabled) = command {
        service::load_config(state)?;
        crate::autostart::for_profile(state, binary)?.set_enabled(enabled)?;
        return Ok(if enabled {
            "Profil ini akan dimulai saat login Linux."
        } else {
            "Mulai saat login dinonaktifkan untuk profil ini."
        });
    }
    if matches!(command, Command::Start) {
        // Never start a second writer when a live control endpoint is already responding.
        if read_snapshot(state).await.live {
            return Ok("Service sudah berjalan.");
        }
        let config = service::load_config(state)?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(state.join("service-output.log"))?;
        let stderr = log.try_clone()?;
        let mut child = std::process::Command::new(binary)
            .arg("--state")
            .arg(state)
            .arg("--account")
            .arg(&config.account)
            .arg("run")
            .stdin(std::process::Stdio::null())
            .stdout(log)
            .stderr(stderr)
            .spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if read_snapshot(state).await.live {
                return Ok("Service aktif. Jeda sebelumnya tetap dipertahankan.");
            }
        }
        return Err(Error::Invalid("service startup not confirmed".into()));
    }
    let current = service::send(state, &Request::new(Action::Status)).await?;
    let action = match command {
        Command::Pause => Action::Pause,
        Command::Resume => Action::Resume,
        Command::SyncNow => Action::SyncNow,
        Command::Shutdown | Command::ShutdownAndWait => Action::Shutdown,
        Command::Start | Command::SetAutostart(_) => unreachable!(),
    };
    let mut request = Request::new(action);
    request.session_id = Some(current.status.session_id);
    // Retry only the same request/session ID. Do not turn an uncertain ACK into a new action.
    let reply = match service::send(state, &request).await {
        Ok(r) => r,
        Err(_) => service::send(state, &request).await?,
    };
    if !reply.accepted {
        return Err(Error::Invalid("service rejected action".into()));
    }
    if matches!(command, Command::ShutdownAndWait) {
        // An ACK means draining, not exited. Wait for the single-writer lock to be released.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        while !engine_available(state)? {
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Invalid("service shutdown not confirmed".into()));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        return Ok("Service berhenti dengan tertib.");
    }
    Ok(match command {
        Command::Pause => "Jeda diterima. Siklus aktif diselesaikan terlebih dahulu.",
        Command::Resume => "Sinkronisasi dilanjutkan.",
        Command::SyncNow => "Sinkronisasi telah dijadwalkan.",
        Command::Shutdown => "Service akan berhenti setelah siklus aktif selesai.",
        Command::ShutdownAndWait => unreachable!(),
        Command::Start | Command::SetAutostart(_) => unreachable!(),
    })
}
fn engine_available(state: &Path) -> Result<bool> {
    use fs2::FileExt;
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(state.join("engine.lock"))
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e.into()),
    };
    match file.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Clone)]
pub struct ActivityItem {
    pub id: String,
    pub path: String,
    pub operation: String,
    pub detail: String,
    pub pending: bool,
    pub retrying: bool,
}
#[derive(Debug, Clone, Default)]
pub struct ActivityPage {
    pub items: Vec<ActivityItem>,
    pub pending_files: usize,
    pub pending_deletions: usize,
    pub retry_operations: usize,
}
/// Counts cover the whole queue, while the visible list stays bounded.
pub async fn read_activity(state: &Path) -> Result<Vec<ActivityItem>> {
    Ok(read_activity_page(state).await?.items)
}
pub async fn read_activity_page(state: &Path) -> Result<ActivityPage> {
    let config = service::load_config(state)?;
    let db = crate::db::Database::read_only(&state.join("state.sqlite3"), &config.account).await?;
    let mut tx = db.pool.begin().await?;
    let local="(f.sync_status IN ('new_local','modified_local','deleted_local') OR json_extract(f.data,'$.metadata_issue') IS NOT NULL) AND coalesce(json_extract(f.data,'$.review_required'),0)=0";
    let count_sql=format!("SELECT count(*) FROM (SELECT file_id FROM sync_operations WHERE stage NOT IN ('done','cancelled') UNION SELECT f.id FROM files f WHERE {local})");
    let pending_files = sqlx::query_scalar::<_, i64>(&count_sql)
        .fetch_one(&mut *tx)
        .await? as usize;
    let delete_sql=format!("SELECT count(*) FROM (SELECT file_id FROM sync_operations WHERE stage NOT IN ('done','cancelled') AND json_extract(data,'$.kind') IN ('trash','remote_trash') UNION SELECT f.id FROM files f WHERE {local} AND f.sync_status='deleted_local')");
    let pending_deletions = sqlx::query_scalar::<_, i64>(&delete_sql)
        .fetch_one(&mut *tx)
        .await? as usize;
    let retry_operations=sqlx::query_scalar::<_,i64>("SELECT (SELECT count(*) FROM sync_operations WHERE stage NOT IN ('done','cancelled') AND json_extract(data,'$.last_error') IS NOT NULL) + (SELECT count(*) FROM files f WHERE json_extract(f.data,'$.metadata_issue') IS NOT NULL AND NOT EXISTS (SELECT 1 FROM sync_operations o WHERE o.file_id=f.id AND o.stage NOT IN ('done','cancelled') AND json_extract(o.data,'$.last_error') IS NOT NULL))").fetch_one(&mut *tx).await? as usize;
    let mut result = Vec::new();
    for done in [false, true] {
        let query = if done {
            "SELECT o.data,f.relative_path FROM sync_operations o JOIN files f ON f.id=o.file_id WHERE o.stage='done' ORDER BY o.rowid DESC LIMIT 10"
        } else {
            "SELECT o.data,f.relative_path FROM sync_operations o JOIN files f ON f.id=o.file_id WHERE o.stage NOT IN ('done','cancelled') ORDER BY o.rowid DESC LIMIT 30"
        };
        let rows: Vec<(String, String)> = sqlx::query_as(query).fetch_all(&mut *tx).await?;
        for (data, path) in rows {
            let op: crate::model::Operation = serde_json::from_str(&data)?;
            let operation = match op.kind.as_str() {
                "create" | "upload" => "Unggah",
                "download" => "Unduh",
                "conflict" => "Simpan salinan konflik",
                "rename" | "remote_move" => "Pindah / ubah nama",
                "trash" | "remote_trash" => "Hapus",
                _ => "Sinkronisasi",
            }
            .to_string();
            let detail = if done {
                "Selesai".into()
            } else if let Some(error) = &op.last_error {
                crate::diagnostics::FileIssue::from_legacy(error)
                    .description()
                    .into()
            } else {
                match op.stage.as_str() {
                    "planned" => "Dalam antrean",
                    "snapshot_ready" => "Menyiapkan / mengirim konten",
                    "remote_committed" => "Memverifikasi hasil di Drive",
                    "downloaded" => "Memasang file lokal",
                    _ => "Sedang diproses",
                }
                .into()
            };
            result.push(ActivityItem {
                id: op.id,
                path,
                operation,
                detail,
                pending: !done,
                retrying: !done && op.last_error.is_some(),
            });
        }
        if !done && result.len() < 30 {
            let query=format!("SELECT f.data FROM files f WHERE {local} AND NOT EXISTS (SELECT 1 FROM sync_operations o WHERE o.file_id=f.id AND o.stage NOT IN ('done','cancelled')) ORDER BY f.relative_path LIMIT ?");
            let rows: Vec<String> = sqlx::query_scalar(&query)
                .bind((30 - result.len()) as i64)
                .fetch_all(&mut *tx)
                .await?;
            for data in rows {
                let file: crate::model::LocalFile = serde_json::from_str(&data)?;
                result.push(ActivityItem {
                    id: format!("local:{}:{}", file.id, file.generation),
                    path: file.relative_path,
                    operation: if file.status == "deleted_local" {
                        "Hapus"
                    } else {
                        "Unggah / ubah"
                    }
                    .into(),
                    detail: file
                        .metadata_issue
                        .map(|issue| issue.description())
                        .unwrap_or("Perubahan tercatat — menunggu giliran sinkronisasi")
                        .into(),
                    pending: true,
                    retrying: file.metadata_issue.is_some(),
                });
            }
        }
    }
    tx.commit().await?;
    db.pool.close().await;
    Ok(ActivityPage {
        items: result,
        pending_files,
        pending_deletions,
        retry_operations,
    })
}
