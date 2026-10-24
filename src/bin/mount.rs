use clap::Parser;
use fs2::FileExt;
use mygdrive_sync::{
    virtual_drive::{validate_mountpoint, DriveFs},
    Result,
};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::PathBuf, time::Duration};
#[derive(Parser)]
#[command(about = "MyGdrive Rust online namespace for Thunar; keep this process running")]
struct Args {
    #[arg(long)]
    state: PathBuf,
    #[arg(long, default_value = "default")]
    account: String,
    #[arg(long)]
    mount: PathBuf,
}
fn run() -> Result<()> {
    let a = Args::parse();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(a.state.join("online-drive.lock"))?;
    lock.try_lock_exclusive().map_err(|_| {
        mygdrive_sync::Error::Invalid("online drive is already running for this profile".into())
    })?;
    let mut fs = DriveFs::new(&a.state, &a.account)?;
    let mount = validate_mountpoint(&a.mount, fs.backing_root(), &a.state)?;
    fs.enable_emblems(&mount)?;
    let session = fuser::spawn_mount2(
        fs,
        &mount,
        &[
            fuser::MountOption::FSName("mygdrive".into()),
            fuser::MountOption::DefaultPermissions,
            fuser::MountOption::NoSuid,
            fuser::MountOption::NoDev,
            fuser::MountOption::NoExec,
        ],
    )?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal handler");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{},_=async {while !session.guard.is_finished() {tokio::time::sleep(Duration::from_secs(1)).await;}}=>{}}
    });
    session.join();
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
