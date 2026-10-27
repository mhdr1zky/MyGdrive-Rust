//! Graphical setup backend. New pairs are isolated; setup never starts a sync cycle.
use crate::i18n::{fmt, tr};
use crate::{
    drive::AccessTokenProvider,
    oauth,
    service::{self, ServiceConfig},
    Error, Result,
};
use fs2::FileExt;
use futures_util::StreamExt;
use serde::Deserialize;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Deserialize)]
pub struct Folder {
    pub id: String,
    pub name: String,
}
#[derive(Debug, Deserialize)]
pub struct FolderPage {
    #[serde(default)]
    pub files: Vec<Folder>,
    #[serde(rename = "nextPageToken")]
    pub next: Option<String>,
}
pub struct FolderBrowser {
    http: reqwest::Client,
    endpoint: reqwest::Url,
    token: Arc<dyn AccessTokenProvider>,
}
impl FolderBrowser {
    pub fn new(token: Arc<dyn AccessTokenProvider>) -> Result<Self> {
        Self::with_endpoint(token, "https://www.googleapis.com/drive/v3/files")
    }
    /// Custom endpoints only support HTTPS or loopback HTTP, for protocol tests.
    pub fn with_endpoint(token: Arc<dyn AccessTokenProvider>, endpoint: &str) -> Result<Self> {
        let endpoint =
            reqwest::Url::parse(endpoint).map_err(|_| Error::Invalid("API URL".into()))?;
        if endpoint.scheme() != "https"
            && !(endpoint.scheme() == "http"
                && matches!(
                    endpoint.host_str(),
                    Some("127.0.0.1" | "localhost" | "[::1]")
                ))
        {
            return Err(Error::Invalid("insecure folder API".into()));
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            endpoint,
            token,
        })
    }
    pub async fn list(&self, parent: &str, page: Option<&str>) -> Result<FolderPage> {
        validate_id(parent)?;
        let q=format!("'{parent}' in parents and mimeType = 'application/vnd.google-apps.folder' and trashed = false");
        let mut params = vec![
            ("q", q.as_str()),
            ("spaces", "drive"),
            ("corpora", "user"),
            ("pageSize", "100"),
            ("orderBy", "folder,name"),
            ("fields", "nextPageToken,files(id,name)"),
        ];
        if let Some(p) = page {
            params.push(("pageToken", p));
        }
        self.json(self.http.get(self.endpoint.clone()).query(&params))
            .await
    }
    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T> {
        let response = request
            .bearer_auth(self.token.access_token().await?)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(if response.status().as_u16() == 401 {
                Error::AuthRequired
            } else {
                Error::Http {
                    status: response.status().as_u16(),
                    reason: "folder request failed".into(),
                }
            });
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                return Err(Error::Invalid("folder response too large".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub async fn validate_folder(&self, id: &str) -> Result<Folder> {
        validate_id(id)?;
        #[derive(Deserialize)]
        struct Metadata {
            id: String,
            name: String,
            #[serde(rename = "mimeType")]
            mime: String,
            #[serde(default)]
            trashed: bool,
            #[serde(rename = "driveId")]
            drive: Option<String>,
        }
        let mut url = self.endpoint.clone();
        url.path_segments_mut()
            .map_err(|_| Error::Invalid("API URL".into()))?
            .push(id);
        let m: Metadata = self
            .json(
                self.http
                    .get(url)
                    .query(&[("fields", "id,name,mimeType,trashed,driveId")]),
            )
            .await?;
        if m.mime != "application/vnd.google-apps.folder" || m.trashed || m.drive.is_some() {
            return Err(Error::Invalid(
                tr("Pilih folder My Drive yang masih tersedia. Shared Drive belum didukung.")
                    .into(),
            ));
        }
        validate_id(&m.id)?;
        Ok(Folder {
            id: m.id,
            name: m.name,
        })
    }
}
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::Invalid("invalid folder ID".into()));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub state: PathBuf,
    pub config: ServiceConfig,
}
fn registered(state: &Path) -> Result<Option<ServiceConfig>> {
    match fs::symlink_metadata(service::config_path(state)) {
        Ok(_) => Ok(Some(service::load_config(state)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub fn profiles(base: &Path) -> Result<Vec<Profile>> {
    let mut result = Vec::new();
    if let Some(config) = registered(base)? {
        result.push(Profile {
            state: base.into(),
            config,
        });
    }
    let entries = match fs::read_dir(base.join("profiles")) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let e = entry?;
        if !e.file_type()?.is_dir() {
            continue;
        }
        if let Some(config) = registered(&e.path())? {
            result.push(Profile {
                state: e.path(),
                config,
            });
        }
    }
    result.sort_by(|a, b| a.config.root.cmp(&b.config.root));
    Ok(result)
}
/// Requires an existing empty local directory. Saves configuration without starting uploads.
pub fn save_pair(base: &Path, state: &Path, root: &Path, folder: &Folder) -> Result<ServiceConfig> {
    validate_id(&folder.id)?;
    crate::local::private_dir(base)?;
    let base = base.canonicalize()?;
    crate::local::private_dir(&base.join("profiles"))?;
    let state = state.canonicalize()?;
    if state.parent() != Some(base.join("profiles").as_path()) {
        return Err(Error::Invalid(
            "profile must be a direct child of profiles directory".into(),
        ));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(base.join("profiles.lock"))?;
    lock.try_lock_exclusive()
        .map_err(|_| Error::Invalid(tr("Pengaturan profil sedang dipakai. Coba lagi.").into()))?;
    let root = root.canonicalize()?;
    if !root.is_dir() || root.starts_with(&base) || base.starts_with(&root) {
        return Err(Error::Invalid(
            tr("Folder lokal harus terpisah dari penyimpanan aplikasi.").into(),
        ));
    }
    for p in profiles(&base)? {
        let other = p.config.root.canonicalize()?;
        if other.starts_with(&root) || root.starts_with(&other) {
            return Err(Error::Invalid(
                tr("Folder lokal bertumpang tindih dengan profil tersimpan.").into(),
            ));
        }
    }
    if fs::read_dir(&root)?.next().is_some() {
        return Err(Error::Invalid(
            tr("Pilih folder lokal kosong untuk pasangan baru.").into(),
        ));
    }
    if fs::symlink_metadata(&state)?.uid() != unsafe { libc::geteuid() } {
        return Err(Error::Invalid("profile ownership".into()));
    }
    crate::local::private_dir(&state)?;
    let config = ServiceConfig {
        version: 1,
        account: "default".into(),
        root,
        drive_folder: folder.id.clone(),
        interval_seconds: crate::service::DEFAULT_POLL_SECONDS,
        cache_max_bytes: None,
    };
    config.validate()?;
    // Link a complete, durable temporary file into place without replacing any configuration.
    let temp = state.join(format!(".setup-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        f.write_all(&serde_json::to_vec_pretty(&config)?)?;
        f.sync_all()?;
        fs::hard_link(&temp, service::config_path(&state))?;
        File::open(&state)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(temp);
    result?;
    Ok(config)
}

#[derive(Debug, Clone)]
pub struct Account {
    pub state: PathBuf,
    pub identity: Option<oauth::AccountIdentity>,
    pub config: Option<ServiceConfig>,
}
pub fn accounts(base: &Path) -> Result<Vec<Account>> {
    let mut paths = vec![base.to_path_buf()];
    if let Ok(entries) = fs::read_dir(base.join("profiles")) {
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                paths.push(entry.path());
            }
        }
    }
    let mut accounts = Vec::new();
    for state in paths {
        let config = registered(&state)?;
        let label = config
            .as_ref()
            .map(|c| c.account.as_str())
            .unwrap_or("default");
        let token = oauth::token_path(&state, label);
        if oauth::saved_identity(&token).is_ok() || config.is_some() {
            let identity = oauth::registered_identity(&token);
            accounts.push(Account {
                state,
                identity,
                config,
            });
        }
    }
    Ok(accounts)
}
pub enum Command {
    Login(PathBuf),
    AddAccount,
    Reauthenticate,
    UseAccount(PathBuf),
    Browse {
        parent: String,
        page: Option<String>,
    },
    Save {
        root: PathBuf,
        folder: String,
    },
}
pub enum Event {
    SignedOut {
        identity: Option<oauth::AccountIdentity>,
        config: Option<ServiceConfig>,
        saved_client: bool,
    },
    Connected {
        identity: Option<oauth::AccountIdentity>,
        config: Option<ServiceConfig>,
        folder: Option<Folder>,
    },
    Accounts(Vec<Account>),
    Folders(FolderPage),
    Saved(PathBuf),
    Profiles(Vec<Profile>),
    LoginRequired,
    Error(String),
}
pub struct Controller {
    pub commands: tokio::sync::mpsc::Sender<Command>,
    pub events: mpsc::Receiver<Event>,
    stop: CancellationToken,
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
impl Controller {
    pub fn stop(&self) {
        self.stop.cancel();
    }
}

async fn connected(state: &Path) -> Result<Event> {
    let config = registered(state)?;
    let account = config
        .as_ref()
        .map(|c| c.account.as_str())
        .unwrap_or("default");
    let token = oauth::token_path(state, account);
    if matches!(oauth::saved_identity(&token), Err(Error::AuthRequired)) {
        return Ok(Event::SignedOut {
            identity: oauth::registered_identity(&token),
            config,
            saved_client: oauth::has_saved_client(&token),
        });
    }
    let mut identity = oauth::saved_identity(&token)?;
    if identity.is_none() {
        identity = Some(oauth::refresh_identity(&token).await?);
    }
    let folder = if let Some(c) = &config {
        if c.drive_folder == "root" {
            Some(Folder {
                id: "root".into(),
                name: tr("My Drive (seluruh drive)").into(),
            })
        } else {
            let browser = FolderBrowser::new(Arc::new(oauth::OAuthProvider::new(token)?))?;
            Some(
                browser
                    .validate_folder(&c.drive_folder)
                    .await
                    .unwrap_or(Folder {
                        id: c.drive_folder.clone(),
                        name: fmt(
                            "Folder tersimpan ({})",
                            &[("0", (c.drive_folder).to_string())],
                        ),
                    }),
            )
        }
    } else {
        None
    };
    Ok(Event::Connected {
        identity,
        config,
        folder,
    })
}
async fn execute(base: &Path, state: &mut PathBuf, command: Command) -> Result<Event> {
    match command {
        Command::AddAccount => {
            let account = registered(state)?
                .map(|config| config.account)
                .unwrap_or_else(|| "default".into());
            let source = oauth::token_path(state, &account);
            let draft = base.join("profiles").join(uuid::Uuid::new_v4().to_string());
            crate::local::private_dir(base)?;
            crate::local::private_dir(&base.join("profiles"))?;
            crate::local::private_dir(&draft)?;
            let target = oauth::token_path(&draft, "default");
            let result = async {
                oauth::prepare_new_account(&source, &target)?;
                oauth::login_desktop(None, &target).await
            }
            .await;
            if let Err(error) = result {
                let _ = fs::remove_dir_all(&draft);
                return Err(error);
            }
            *state = draft;
            connected(state).await
        }
        Command::Login(client) => {
            let draft = base.join("profiles").join(uuid::Uuid::new_v4().to_string());
            crate::local::private_dir(base)?;
            crate::local::private_dir(&base.join("profiles"))?;
            crate::local::private_dir(&draft)?;
            let result =
                oauth::login_desktop(Some(&client), &oauth::token_path(&draft, "default")).await;
            if let Err(error) = result {
                let _ = fs::remove_dir_all(&draft);
                return Err(error);
            }
            *state = draft;
            connected(state).await
        }
        Command::Reauthenticate => {
            let account = service::load_config(state)
                .map(|c| c.account)
                .unwrap_or_else(|_| "default".into());
            oauth::login_desktop(None, &oauth::token_path(state, &account)).await?;
            connected(state).await
        }
        Command::UseAccount(target) => {
            if !accounts(base)?.iter().any(|a| a.state == target) {
                return Err(Error::Invalid(
                    tr("Akun tidak ditemukan pada daftar profil.").into(),
                ));
            }
            *state = target;
            connected(state).await
        }
        Command::Browse { parent, page } => {
            let account = registered(state)?
                .map(|c| c.account)
                .unwrap_or_else(|| "default".into());
            let browser = FolderBrowser::new(Arc::new(oauth::OAuthProvider::new(
                oauth::token_path(state, &account),
            )?))?;
            Ok(Event::Folders(
                browser.list(&parent, page.as_deref()).await?,
            ))
        }
        Command::Save { root, folder } => {
            if registered(state)?.is_some() {
                return Err(Error::Invalid(tr("Profil ini sudah tersimpan. Hubungkan akun baru untuk membuat pasangan terpisah.").into()));
            }
            let browser = FolderBrowser::new(Arc::new(oauth::OAuthProvider::new(
                oauth::token_path(state, "default"),
            )?))?;
            let folder = browser.validate_folder(&folder).await?;
            save_pair(base, state, &root, &folder)?;
            Ok(Event::Saved(state.to_path_buf()))
        }
    }
}
fn safe_error(error: Error) -> Event {
    match error {
        Error::AuthRequired => Event::LoginRequired,
        Error::Invalid(s) => Event::Error(s),
        _ => Event::Error(
            tr("Operasi gagal. Periksa koneksi, file client OAuth, dan izin folder lalu coba lagi.")
                .into(),
        ),
    }
}
pub fn spawn(base: PathBuf, current: PathBuf) -> Result<Controller> {
    spawn_observer(base, current, false)
}
pub fn spawn_observer(base: PathBuf, current: PathBuf, read_only: bool) -> Result<Controller> {
    let (commands, mut input) = tokio::sync::mpsc::channel(4);
    let (events, receiver) = mpsc::channel();
    let stop = CancellationToken::new();
    let cancel = stop.clone();
    std::thread::Builder::new()
        .name("mygdrive-setup".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => {
                    let _ = events.send(Event::Error(tr("Worker pengaturan gagal dimulai.").into()));
                    return;
                }
            };
            runtime.block_on(async move {
                let mut state = current.clone();
                if let Ok(list) = accounts(&base) {
                    let _ = events.send(Event::Accounts(list));
                }
                let account = service::load_config(&current)
                    .map(|c| c.account)
                    .unwrap_or_else(|_| "default".into());
                if oauth::saved_identity(&oauth::token_path(&current, &account)).is_ok() {
                    let initial = tokio::select! {
                        _=cancel.cancelled()=>return,
                        result=async {
                        if read_only {
                            let config=registered(&current)?;
                            let identity=oauth::saved_identity(&oauth::token_path(&current,&account))?;
                            let folder=config.as_ref().map(|c|Folder{id:c.drive_folder.clone(),name:if c.drive_folder=="root"{tr("My Drive (seluruh drive)").into()}else{c.drive_folder.clone()}});
                            Ok(Event::Connected {identity,config,folder})
                        } else {connected(&current).await}
                    }=>result,
                    };
                    let _ = events.send(initial.unwrap_or_else(safe_error));
                } else if registered(&current).ok().flatten().is_some() {
                    let _ = events.send(Event::SignedOut {
                        identity: oauth::registered_identity(&oauth::token_path(&current, &account)),
                        config: registered(&current).ok().flatten(),
                        saved_client: oauth::has_saved_client(&oauth::token_path(&current, &account)),
                    });
                }
                loop {
                    let command = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        command = input.recv() => match command { Some(c) => c, None => break },
                    };
                    if read_only { continue; }
                let result = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        result = execute(&base, &mut state, command) => result,
                    };
                    if events.send(result.unwrap_or_else(safe_error)).is_err() {
                        break;
                    }
                    if let Ok(list) = accounts(&base) {
                        let _ = events.send(Event::Accounts(list));
                    }
                }
            });
            // Authenticated accounts persist even before the folder pair is saved.
        })?;
    Ok(Controller {
        commands,
        events: receiver,
        stop,
    })
}
