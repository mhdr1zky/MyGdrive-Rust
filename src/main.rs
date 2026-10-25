use clap::{Parser, Subcommand};
use mygdrive_sync::{
    db::Database,
    drive::{AccessTokenProvider, DriveClient, HttpDriveClient, TokenFile},
    oauth::{self, OAuthProvider},
    service::{self, Action, Request, ServiceConfig},
    Error, Result, SyncEngine,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(version, about = "Offline-first Google Drive sync (Linux)")]
struct Args {
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long)]
    state: Option<PathBuf>,
    #[arg(long, default_value = "default")]
    account: String,
    #[arg(long)]
    drive_folder: Option<String>,
    #[arg(long, env = "MYGDRIVE_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Preview or recover/apply an offline, journaled expansion to streamed My Drive.
    ExpandMyDrive {
        #[arg(long)]
        apply: bool,
    },
    /// Apply offline intent from Thunar to a selection in the local or virtual drive.
    OfflinePath {
        #[arg(long, value_parser = ["available", "unpin"])]
        mode: String,
        #[arg(long)]
        mount: Option<PathBuf>,
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Control the running service through its private Unix socket.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
        /// Reuse this UUID to safely retry a command within one service session.
        #[arg(long)]
        request_id: Option<uuid::Uuid>,
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Add the local sync folder to Thunar's Places sidebar (no Drive access).
    IntegrateThunar {
        #[arg(long, default_value = "MyGdrive Rust")]
        label: String,
    },
    /// Login in the system browser and store tokens privately for this state/account.
    Login {
        #[arg(long)]
        client_json: PathBuf,
        /// drive: all accessible Drive files; drive-file: only files authorized for this app.
        #[arg(long, value_parser = ["drive", "drive-file"])]
        scope: String,
        #[arg(long)]
        no_browser: bool,
        /// Time allowed to complete consent in the browser (60-3600 seconds).
        #[arg(long, default_value_t = 900, value_parser = clap::value_parser!(u64).range(60..=3600))]
        timeout_seconds: u64,
    },
    /// Persist local changes without accessing Google Drive.
    Scan,
    /// Print persistent per-file status as JSON.
    Status,
    /// Run one reconciliation cycle.
    Sync,
    /// Allow a reviewed recovery copy to be synchronized on the next cycle.
    ApproveRecovery {
        file_id: String,
    },
    /// Protect an already materialized file and its cache from eviction.
    Pin {
        file_id: String,
    },
    Unpin {
        file_id: String,
    },
    /// Remove eligible clean cache objects in LRU order, down to a byte budget.
    CachePrune {
        #[arg(long)]
        max_bytes: u64,
    },
    /// Keep watching offline; retry Drive synchronization periodically.
    Run {
        #[arg(long)]
        interval: Option<u64>,
        #[arg(long)]
        cache_max_bytes: Option<u64>,
        /// Start watcher and IPC without starting any network cycles.
        #[arg(long)]
        paused: bool,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    Status,
    Pause,
    Resume,
    SyncNow,
    Shutdown,
}
impl ServiceAction {
    fn action(&self) -> Action {
        match self {
            Self::Status => Action::Status,
            Self::Pause => Action::Pause,
            Self::Resume => Action::Resume,
            Self::SyncNow => Action::SyncNow,
            Self::Shutdown => Action::Shutdown,
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mygdrive_sync=info".into()),
        )
        .init();
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    let state = match args.state {
        Some(path) => path,
        None => directories::BaseDirs::new()
            .ok_or_else(|| Error::Invalid("cannot resolve user data directory".into()))?
            .data_local_dir()
            .join("mygdrive"),
    };
    if let Command::OfflinePath { mode, mount, paths } = &args.command {
        let n = mygdrive_sync::thunar::set_paths(
            &state,
            &args.account,
            mount.as_deref(),
            paths,
            mode == "available",
        )
        .await?;
        println!("Pilihan tersimpan untuk {n} item. Konten menunggu koneksi dan layanan yang tidak dijeda.");
        return Ok(());
    }
    if let Command::Service {
        action,
        request_id,
        session_id,
    } = &args.command
    {
        let mut request = Request::new(action.action());
        if let Some(id) = request_id {
            request.request_id = id.to_string();
        }
        request.session_id = session_id.clone();
        if request.action != Action::Status && request.session_id.is_none() {
            request.session_id = Some(
                service::send(&state, &Request::new(Action::Status))
                    .await?
                    .status
                    .session_id,
            );
        }
        match service::send(&state, &request).await {
            Ok(reply) => {
                println!("{}", serde_json::to_string_pretty(&reply)?);
                if !reply.accepted {
                    return Err(Error::Invalid(
                        "service rejected command; see response".into(),
                    ));
                }
            }
            Err(Error::Io(e))
                if action.action() == Action::Status
                    && matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
            {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"running":false,"last_known":service::last_status(&state).ok()})
                    )?
                );
            }
            Err(e) => return Err(e),
        }
        return Ok(());
    }
    let oauth_path = oauth::token_path(&state, &args.account);
    if let Command::Login {
        client_json,
        scope,
        no_browser,
        timeout_seconds,
    } = &args.command
    {
        if args.token_file.is_some() {
            return Err(Error::Invalid(
                "--token-file cannot be used with login".into(),
            ));
        }
        let scope = if scope == "drive" {
            oauth::DRIVE_SCOPE
        } else {
            oauth::FILE_SCOPE
        };
        return oauth::login(
            client_json,
            &oauth_path,
            scope,
            *no_browser,
            Duration::from_secs(*timeout_seconds),
        )
        .await;
    }
    if let Command::ExpandMyDrive { apply } = args.command {
        let plan = if apply
            && mygdrive_sync::scope_migration::pending(&state, &args.account).await?
        {
            mygdrive_sync::scope_migration::resume(&state, &args.account).await?
        } else {
            let provider: Arc<dyn AccessTokenProvider> = match &args.token_file {
                Some(path) => Arc::new(TokenFile(path.clone())),
                None if oauth_path.try_exists()? => {
                    Arc::new(OAuthProvider::new(oauth_path.clone())?)
                }
                None => Arc::new(TokenFile(state.join("access.token"))),
            };
            let drive = HttpDriveClient::new(provider)?;
            let remote_root = drive.get("root").await?.ok_or(Error::AuthRequired)?;
            let plan = mygdrive_sync::scope_migration::preview(&state, &args.account, &remote_root)
                .await?;
            if apply && !plan.already_my_drive {
                mygdrive_sync::scope_migration::begin(&state, &args.account, &remote_root).await?;
                mygdrive_sync::scope_migration::resume(&state, &args.account).await?
            } else {
                plan
            }
        };
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }
    let saved = if args.root.is_none() {
        Some(service::load_config(&state)?)
    } else {
        None
    };
    if saved.as_ref().is_some_and(|c| c.account != args.account) {
        return Err(Error::Invalid(
            "saved service belongs to another account; specify --account".into(),
        ));
    }
    let root = args
        .root
        .or_else(|| saved.as_ref().map(|c| c.root.clone()))
        .ok_or_else(|| Error::Invalid("--root is required for sync commands".into()))?;
    let drive_folder = args
        .drive_folder
        .or_else(|| saved.as_ref().map(|c| c.drive_folder.clone()))
        .unwrap_or_else(|| "root".into());
    if let Command::IntegrateThunar { label } = &args.command {
        let dirs = directories::BaseDirs::new()
            .ok_or_else(|| Error::Invalid("cannot resolve user config directory".into()))?;
        let path =
            mygdrive_sync::desktop::install_thunar_shortcut(&root, label, dirs.config_dir())?;
        println!("Thunar shortcut ready: {}", path.display());
        return Ok(());
    }
    if matches!(args.command, Command::Status) {
        let db = Database::read_only(&state.join("state.sqlite3"), &args.account).await?;
        println!("{}", serde_json::to_string_pretty(&db.files().await?)?);
        return Ok(());
    }
    let provider: Arc<dyn AccessTokenProvider> = match args.token_file {
        Some(path) => Arc::new(TokenFile(path)),
        None if oauth_path.try_exists()? => Arc::new(OAuthProvider::new(oauth_path)?),
        None => Arc::new(TokenFile(state.join("access.token"))),
    };
    let drive = Arc::new(HttpDriveClient::new(provider)?);
    let engine =
        Arc::new(SyncEngine::open(&root, &state, &args.account, &drive_folder, drive).await?);
    match args.command {
        Command::Login { .. }
        | Command::IntegrateThunar { .. }
        | Command::Service { .. }
        | Command::OfflinePath { .. }
        | Command::ExpandMyDrive { .. } => {
            unreachable!("setup handled above")
        }
        Command::Scan => {
            engine.recover_moves().await?;
            engine.recover_displaced().await?;
            engine.scanner.scan().await?;
            println!("Local scan persisted.");
        }
        Command::Status => println!(
            "{}",
            serde_json::to_string_pretty(&engine.db.files().await?)?
        ),
        Command::Sync => println!(
            "{}",
            serde_json::to_string_pretty(&engine.sync_once().await?)?
        ),
        Command::ApproveRecovery { file_id } => {
            engine.approve_recovery(&file_id).await?;
            println!("Recovery approved for synchronization.");
        }
        Command::Pin { file_id } => {
            engine.set_available_offline(&file_id, true).await?;
            println!("File pinned offline.");
        }
        Command::Unpin { file_id } => {
            engine.set_available_offline(&file_id, false).await?;
            println!(
                "Offline pin removed; safe local cleanup queued for the next successful sync."
            );
        }
        Command::CachePrune { max_bytes } => println!(
            "{}",
            serde_json::to_string_pretty(&engine.prune_cache(max_bytes).await?)?
        ),
        Command::Run {
            interval,
            cache_max_bytes,
            paused,
        } => {
            let config = ServiceConfig {
                version: 1,
                account: args.account,
                root: engine.scanner.root.clone(),
                drive_folder,
                interval_seconds: interval
                    .or_else(|| saved.as_ref().map(|c| c.interval_seconds))
                    .unwrap_or(service::DEFAULT_POLL_SECONDS),
                cache_max_bytes: cache_max_bytes
                    .or_else(|| saved.as_ref().and_then(|c| c.cache_max_bytes)),
            };
            let stop = CancellationToken::new();
            let signal_stop = stop.clone();
            let signals = tokio::spawn(async move {
                if let Ok(mut term) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                {
                    tokio::select! { _ = tokio::signal::ctrl_c()=>{}, _ = term.recv()=>{} }
                } else {
                    let _ = tokio::signal::ctrl_c().await;
                }
                signal_stop.cancel();
            });
            let result = service::run(engine, &state, config, paused, stop).await;
            signals.abort();
            result?;
        }
    }
    Ok(())
}
