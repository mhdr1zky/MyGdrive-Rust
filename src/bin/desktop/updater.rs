use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::Command, time::Duration};

fn installation() -> Option<(&'static str, PathBuf)> {
    let executable = std::env::current_exe().ok()?;
    if let (Some(image), Some(app_dir)) = (std::env::var_os("APPIMAGE"), std::env::var_os("APPDIR"))
    {
        if executable.starts_with(PathBuf::from(app_dir)) {
            return Some(("appimage", PathBuf::from(image)));
        }
    }
    (executable == std::path::Path::new("/usr/lib/mygdrive-rust/bin/mygdrive-desktop"))
        .then(|| ("deb", PathBuf::from("/usr/lib/mygdrive-rust")))
}

pub fn status_path() -> Option<PathBuf> {
    let (kind, target) = installation()?;
    let identity = format!("{kind}:{}", target.display());
    let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
    Some(
        directories::BaseDirs::new()?
            .data_local_dir()
            .join("mygdrive/updates")
            .join(&digest[..16])
            .join("status.json"),
    )
}

pub fn start() {
    if std::env::var_os("MYGDRIVE_DISABLE_AUTO_UPDATE").is_some() {
        return;
    }
    let Some((kind, target)) = installation() else {
        return;
    };
    let Some(status) = status_path() else {
        return;
    };
    // All network, disk and authentication work stays off the GTK main thread.
    let _ = std::thread::Builder::new()
        .name("github-updater".into())
        .spawn(move || loop {
            let result = Command::new("python3")
                .args(["-c", include_str!("../../../scripts/auto-update.py")])
                .args(["--kind", kind, "--version", env!("CARGO_PKG_VERSION")])
                .arg("--target")
                .arg(&target)
                .arg("--status")
                .arg(&status)
                .status();
            if let Err(error) = result {
                eprintln!("Tidak dapat menjalankan updater: {error}");
            }
            std::thread::sleep(Duration::from_secs(15 * 60));
        });
}
