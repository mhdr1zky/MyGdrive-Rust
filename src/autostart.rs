//! Per-profile XDG autostart entries. Only files bearing our exact marker are changed.
use crate::{Error, Result};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
const MARKER: &str = "X-MyGDrive-Managed=true";
struct SettingsLock(File);
impl Drop for SettingsLock {
    fn drop(&mut self) {
        // Explicit unlock also releases an inherited descriptor in a concurrent fork
        // before that child reaches exec. Closing only the parent FD can leave it held.
        let _ = FileExt::unlock(&self.0);
    }
}

pub fn profile_id(state: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    format!("{:x}", Sha256::digest(state.as_os_str().as_bytes()))
}
fn quote_arg(value: &Path) -> Result<String> {
    let s = value
        .to_str()
        .ok_or_else(|| Error::Invalid("Path autostart harus UTF-8.".into()))?;
    if s.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "Path autostart mengandung karakter kontrol.".into(),
        ));
    }
    let mut escaped = String::new();
    for c in s.chars() {
        if matches!(c, '\\' | '"' | '`' | '$') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    Ok(format!(
        "\"{}\"",
        escaped.replace('\\', "\\\\").replace('%', "%%")
    ))
}
pub struct Autostart {
    directory: PathBuf,
    state: PathBuf,
    launcher: PathBuf,
    id: String,
}
impl Autostart {
    pub fn new(config_home: &Path, state: &Path, launcher: &Path) -> Result<Self> {
        if !config_home.is_absolute() || !launcher.is_absolute() {
            return Err(Error::Invalid("autostart needs absolute paths".into()));
        }
        let state = state.canonicalize()?;
        quote_arg(&state)?;
        quote_arg(launcher)?;
        if launcher.as_os_str().as_encoded_bytes().contains(&b'=') {
            return Err(Error::Invalid(
                "Path launcher autostart mengandung tanda =.".into(),
            ));
        }
        let id = profile_id(&state);
        Ok(Self {
            directory: config_home.join("autostart"),
            state,
            launcher: launcher.into(),
            id,
        })
    }
    pub fn path(&self) -> PathBuf {
        self.directory
            .join(format!("io.mygdrive.Profile-{}.desktop", self.id))
    }
    fn read(&self) -> Result<Option<String>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.path())
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if !file.metadata()?.is_file() || file.metadata()?.len() > 64 * 1024 {
            return Err(Error::Invalid(
                "Entri autostart tidak aman untuk diubah.".into(),
            ));
        }
        let mut value = String::new();
        file.take(65537).read_to_string(&mut value)?;
        if !value.lines().any(|l| l == MARKER)
            || !value
                .lines()
                .any(|l| l == format!("X-MyGDrive-Profile={}", self.id))
        {
            return Err(Error::Invalid(
                "Entri autostart bukan milik profil ini; tidak ditimpa.".into(),
            ));
        }
        Ok(Some(value))
    }
    pub fn enabled(&self) -> Result<bool> {
        Ok(self
            .read()?
            .is_some_and(|v| !v.lines().any(|l| l == "Hidden=true")))
    }
    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        fs::create_dir_all(&self.directory)?;
        if fs::symlink_metadata(&self.directory)?
            .file_type()
            .is_symlink()
        {
            return Err(Error::Invalid("Direktori autostart berupa symlink.".into()));
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.directory.join(".mygdrive.lock"))?;
        lock.try_lock_exclusive().map_err(|_| {
            Error::Invalid("Pengaturan autostart sedang dipakai. Coba lagi.".into())
        })?;
        let _guard = SettingsLock(lock);
        let previous = self.read()?;
        if !enabled {
            if previous.is_some() {
                fs::remove_file(self.path())?;
                File::open(&self.directory)?.sync_all()?;
            }
            return Ok(());
        }
        let meta = fs::metadata(&self.launcher)?;
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            return Err(Error::Invalid(
                "Launcher aplikasi belum dapat dijalankan.".into(),
            ));
        }
        // GIO checks the executable before expanding %% in it. Use env's literal path
        // as the executable, so field-code expansion applies to the launcher argument.
        // This passes argv directly; there is no shell or command substitution.
        let content=format!("[Desktop Entry]\nType=Application\nName=MyGdrive Rust\nComment=Start this MyGdrive Rust profile at login\nExec=/usr/bin/env -- {} --state {} --background --start-service\nTerminal=false\nHidden=false\n{MARKER}\nX-MyGDrive-Profile={}\n",quote_arg(&self.launcher)?,quote_arg(&self.state)?,self.id);
        let temp = self
            .directory
            .join(format!(".mygdrive-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            f.write_all(content.as_bytes())?;
            f.sync_all()?;
            fs::rename(&temp, self.path())?;
            File::open(&self.directory)?.sync_all()?;
            Ok(())
        })();
        let _ = fs::remove_file(temp);
        result
    }
}
pub fn for_profile(state: &Path, binary: &Path) -> Result<Autostart> {
    if std::env::var_os("FLATPAK_ID").is_some() {
        return Err(Error::Unsupported(
            "Mulai saat login belum tersedia pada paket Flatpak.".into(),
        ));
    }
    let dirs = directories::BaseDirs::new()
        .ok_or_else(|| Error::Invalid("user config directory unavailable".into()))?;
    let launcher = desktop_launcher(binary)?;
    Autostart::new(dirs.config_dir(), state, &launcher)
}
/// Find the dashboard launcher for a source build or an installed binary.
pub fn desktop_launcher(binary: &Path) -> Result<PathBuf> {
    let parent = binary
        .parent()
        .ok_or_else(|| Error::Invalid("binary directory unavailable".into()))?;
    let source_launcher = parent.join("../../run-desktop.sh");
    let installed_launcher = parent.join("../run-desktop.sh");
    if source_launcher.is_file() {
        Ok(source_launcher.canonicalize()?)
    } else if installed_launcher.is_file() {
        Ok(installed_launcher.canonicalize()?)
    } else {
        Ok(parent.join("mygdrive-desktop"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_and_debian_layouts_keep_the_mount_launcher_for_autostart() {
        let temp = tempfile::tempdir().unwrap();
        for relative in ["source/target/release", "installed/bin"] {
            let directory = temp.path().join(relative);
            fs::create_dir_all(&directory).unwrap();
            let launcher = if relative.starts_with("source/") {
                temp.path().join("source/run-desktop.sh")
            } else {
                temp.path().join("installed/run-desktop.sh")
            };
            fs::write(&launcher, "#!/bin/sh\n").unwrap();
            assert_eq!(
                desktop_launcher(&directory.join("mygdrive-sync")).unwrap(),
                launcher
            );
        }
    }
}
