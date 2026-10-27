//! Account collection: isolated profile state, bounded observation and targeted controls.
use crate::{
    dashboard::{self, Snapshot},
    i18n::tr,
    oauth, onboarding, service, Error, Result,
};
use fs2::FileExt;
use futures_util::{stream, StreamExt};
use std::{
    fs::OpenOptions,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub fn base_for(state: &Path) -> PathBuf {
    if state
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "profiles")
    {
        state
            .parent()
            .and_then(Path::parent)
            .expect("profile collection")
            .to_path_buf()
    } else {
        state.to_path_buf()
    }
}

#[derive(Debug, Clone)]
pub struct Account {
    pub state: PathBuf,
    pub identity: Option<oauth::AccountIdentity>,
    pub snapshot: Snapshot,
    pub configured: bool,
    pub signed_out: bool,
}
impl Account {
    pub fn email(&self) -> &str {
        self.identity
            .as_ref()
            .map(|identity| identity.email.as_str())
            .unwrap_or("")
    }
    pub fn title(&self) -> &str {
        self.identity
            .as_ref()
            .and_then(|identity| {
                (!identity.display_name.is_empty()).then_some(identity.display_name.as_str())
            })
            .unwrap_or_else(|| {
                if self.email().is_empty() {
                    tr("Akun Google")
                } else {
                    self.email()
                }
            })
    }
    pub fn status(&self) -> &'static str {
        if self.signed_out {
            tr("Akun terputus")
        } else if !self.configured {
            tr("Selesaikan pengaturan folder")
        } else {
            self.snapshot.title()
        }
    }
}

pub fn validate_target(base: &Path, target: &Path) -> Result<PathBuf> {
    let target = target.canonicalize()?;
    let base = base.canonicalize()?;
    if target == base {
        return Ok(target);
    }
    if onboarding::accounts(&base)?
        .iter()
        .any(|account| account.state == target)
    {
        Ok(target)
    } else {
        Err(Error::Invalid("account is outside this collection".into()))
    }
}

pub async fn list(base: &Path) -> Result<Vec<Account>> {
    let accounts = onboarding::accounts(base)?;
    let mut rows = stream::iter(accounts)
        .map(|account| async move {
            let label = account
                .config
                .as_ref()
                .map(|config| config.account.as_str())
                .unwrap_or("default");
            let signed_out = matches!(
                oauth::saved_identity(&oauth::token_path(&account.state, label)),
                Err(Error::AuthRequired)
            );
            let snapshot = dashboard::read_snapshot(&account.state).await;
            Account {
                state: account.state,
                identity: account.identity,
                configured: account.config.is_some(),
                snapshot,
                signed_out,
            }
        })
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
    rows.sort_by(|a, b| {
        a.email()
            .to_ascii_lowercase()
            .cmp(&b.email().to_ascii_lowercase())
            .then_with(|| a.state.cmp(&b.state))
    });
    Ok(rows)
}

pub async fn control(
    base: &Path,
    state: &Path,
    binary: &Path,
    command: dashboard::Command,
) -> Result<&'static str> {
    let target = validate_target(base, state)?;
    let _start = if matches!(command, dashboard::Command::Start) {
        Some(collection_lock(base)?)
    } else {
        None
    };
    if !matches!(
        command,
        dashboard::Command::Start
            | dashboard::Command::Pause
            | dashboard::Command::Resume
            | dashboard::Command::SyncNow
            | dashboard::Command::ShutdownAndWait
    ) {
        return Err(Error::Invalid("unsupported account control".into()));
    }
    dashboard::perform(&target, binary, command).await
}

fn collection_lock(base: &Path) -> Result<std::fs::File> {
    crate::local::private_dir(base)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(base.join("collection-control.lock"))?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(Error::Changed);
    }
    file.try_lock_exclusive()
        .map_err(|_| Error::Invalid("another account lifecycle command is running".into()))?;
    Ok(file)
}
#[derive(serde::Serialize, serde::Deserialize)]
struct ResumeAccounts {
    version: u32,
    profiles: Vec<PathBuf>,
}
fn resume_path(base: &Path) -> PathBuf {
    base.join("restart-accounts.json")
}
fn read_resume(base: &Path) -> Result<ResumeAccounts> {
    use std::io::Read;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(resume_path(base))?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.mode() & 0o077 != 0
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.len() > 128 * 1024
    {
        return Err(Error::Changed);
    }
    let record: ResumeAccounts = serde_json::from_reader(file.take(128 * 1024 + 1))?;
    if record.version != 1 || record.profiles.len() > 256 {
        return Err(Error::Changed);
    }
    Ok(record)
}
/// Drain every running profile before app quit/update. The restart receipt
/// retains profiles already drained by an earlier, interrupted attempt.
pub async fn drain_collection(base: &Path, binary: &Path, restart: bool) -> Result<()> {
    let _collection = collection_lock(base)?;
    let accounts = list(base).await?;
    let mut running = Vec::new();
    for account in accounts {
        if account.snapshot.live || !dashboard::engine_available(&account.state)? {
            running.push(account.state);
        }
    }
    if restart {
        let mut profiles = match read_resume(base) {
            Ok(record) => record.profiles,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        for state in &running {
            if !profiles.contains(state) {
                profiles.push(state.clone());
            }
        }
        for state in &profiles {
            validate_target(base, state)?;
        }
        service::atomic_json(
            &resume_path(base),
            &ResumeAccounts {
                version: 1,
                profiles,
            },
        )?;
    }
    for state in running {
        dashboard::perform(&state, binary, dashboard::Command::ShutdownAndWait).await?;
    }
    if !restart {
        match std::fs::remove_file(resume_path(base)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
pub async fn restore_collection(base: &Path, binary: &Path) -> Result<()> {
    let record = match read_resume(base) {
        Ok(record) => record,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // Validate all targets before any process is started.
    for state in &record.profiles {
        validate_target(base, state)?;
    }
    for state in record.profiles {
        let config = service::load_config(&state)?;
        if matches!(
            oauth::saved_identity(&oauth::token_path(&state, &config.account)),
            Err(Error::AuthRequired)
        ) {
            continue;
        }
        control(base, &state, binary, dashboard::Command::Start).await?;
    }
    std::fs::remove_file(resume_path(base))?;
    Ok(())
}

pub async fn disconnect(base: &Path, state: &Path, binary: &Path) -> Result<()> {
    let state = validate_target(base, state)?;
    let config = service::load_config(&state)?;
    dashboard::perform(&state, binary, dashboard::Command::ShutdownAndWait).await?;
    // Reserve writer ownership through credential removal; a concurrent service
    // launch must never keep using tokens that the user just disconnected.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(state.join("engine.lock"))?;
    let meta = lock.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(Error::Changed);
    }
    lock.try_lock_exclusive().map_err(|_| Error::Changed)?;
    crate::autostart::for_profile(&state, binary)?.set_enabled(false)?;
    oauth::disconnect_local(&oauth::token_path(&state, &config.account))?;
    Ok(())
}

pub enum Command {
    List,
    Select(PathBuf),
    Control {
        state: PathBuf,
        command: dashboard::Command,
    },
    Disconnect(PathBuf),
}
pub enum Event {
    Accounts(Vec<Account>),
    Selected(PathBuf),
    Completed(std::result::Result<String, String>),
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
        self.stop();
    }
}

pub fn spawn(base: PathBuf, binary: PathBuf, read_only: bool) -> Result<Controller> {
    let (commands, mut input) = tokio::sync::mpsc::channel(8);
    let (events, receiver) = mpsc::channel();
    let stop = CancellationToken::new();
    let cancel = stop.clone();
    std::thread::Builder::new().name("mygdrive-accounts".into()).spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return; };
        runtime.block_on(async move {
            let mut poll = tokio::time::interval(Duration::from_secs(3));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let command = tokio::select! { _=cancel.cancelled()=>break, _=poll.tick()=>Some(Command::List), command=input.recv()=>command };
                let Some(command) = command else { break; };
                let work = async {
                    match command {
                        Command::List => {},
                        Command::Select(state) => {
                            match validate_target(&base,&state) {
                                Ok(state) => { let _=events.send(Event::Selected(state)); },
                                Err(_) => { let _=events.send(Event::Completed(Err(tr("Akun tidak ditemukan. Muat ulang daftar akun.").into()))); }
                            }
                        }
                        Command::Control { state, command } if !read_only => {
                            let result=control(&base,&state,&binary,command).await.map(str::to_owned)
                                .map_err(|_| tr("Perintah akun belum terkonfirmasi. Periksa status lalu coba lagi.").to_owned());
                            let _=events.send(Event::Completed(result));
                        }
                        Command::Disconnect(state) if !read_only => {
                            let result=disconnect(&base,&state,&binary).await.map(|()|tr("Akun diputuskan. File lokal tetap tersimpan.").to_owned())
                                .map_err(|_|tr("Akun belum diputuskan. Tunggu layanan berhenti, lalu coba lagi.").to_owned());
                            let _=events.send(Event::Completed(result));
                        }
                        _ => {},
                    }
                    if let Ok(accounts)=list(&base).await { let _=events.send(Event::Accounts(accounts)); }
                };
                // Disconnect must complete its short credential transaction once
                // drain has started, even if the management window is closed.
                work.await;
                if cancel.is_cancelled() { break; }
            }
        });
    })?;
    Ok(Controller {
        commands,
        events: receiver,
        stop,
    })
}
