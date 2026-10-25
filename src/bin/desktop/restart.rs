//! Relaunch only after the old GTK process (and its single-instance name) exits.
use mygdrive_sync::{Error, Result};
use std::{
    ffi::OsString,
    fs::OpenOptions,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone)]
pub struct Launcher {
    program: PathBuf,
    app_dir: Option<PathBuf>,
    state: PathBuf,
}
pub struct Plan {
    launcher: Launcher,
    language: String,
    pub start_service: bool,
    background: bool,
    progress: bool,
}
impl Launcher {
    pub fn capture(state: &Path, binary: &Path) -> Result<Self> {
        Ok(Self {
            // Capture before package installation can replace the executable inode.
            program: mygdrive_sync::autostart::desktop_launcher(binary)?,
            app_dir: std::env::var_os("APPDIR").map(PathBuf::from),
            state: std::path::absolute(state)?,
        })
    }
    pub fn plan(&self, language: &str, background: bool, progress: bool) -> Result<Plan> {
        let metadata = std::fs::metadata(&self.program)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(Error::Invalid(
                "updated desktop launcher is not executable".into(),
            ));
        }
        Ok(Plan {
            launcher: self.clone(),
            language: language.into(),
            start_service: false,
            background,
            progress,
        })
    }
}
fn without_app_dir(value: &std::ffi::OsStr, app_dir: &Path) -> Result<OsString> {
    std::env::join_paths(std::env::split_paths(value).filter(|path| !path.starts_with(app_dir)))
        .map_err(|_| Error::Invalid("invalid launcher environment path".into()))
}
impl Plan {
    fn command(&self) -> Result<Command> {
        let start = std::fs::read_to_string("/proc/self/stat")?
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(19))
            .ok_or_else(|| Error::Invalid("desktop process identity unavailable".into()))?
            .to_owned();
        let mut command = Command::new("python3");
        command
            .args([
                "-c",
                include_str!("../../../scripts/restart-desktop.py"),
                "--pid",
                &std::process::id().to_string(),
                "--start-time",
                &start,
                "--",
            ])
            .arg(&self.launcher.program)
            .arg("--state")
            .arg(&self.launcher.state)
            .arg("--language")
            .arg(&self.language);
        if self.start_service {
            command.arg("--start-service");
        }
        if self.background {
            command.arg("--background");
        }
        if self.progress {
            command.arg("--progress");
        }
        // A new AppImage must not load GTK/schema/loader files from its old mount.
        if let Some(app_dir) = &self.launcher.app_dir {
            for name in [
                "LD_LIBRARY_PATH",
                "XDG_DATA_DIRS",
                "PATH",
                "GIO_EXTRA_MODULES",
            ] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, without_app_dir(&value, app_dir)?);
                }
            }
            for name in [
                "APPDIR",
                "APPIMAGE",
                "ARGV0",
                "OWD",
                "GSETTINGS_SCHEMA_DIR",
                "GDK_PIXBUF_MODULE_FILE",
            ] {
                command.env_remove(name);
            }
        }
        Ok(command)
    }
    pub fn spawn_after_exit(&self) -> Result<()> {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.launcher.state.join("desktop-restart.log"))?;
        self.command()?
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relaunch_preserves_profile_language_and_running_state_without_shell_parsing() {
        let directory = tempfile::tempdir().unwrap();
        let launcher = Launcher {
            program: PathBuf::from("/path with spaces/MyGdrive.AppImage"),
            app_dir: None,
            state: directory.path().join("profile $ with spaces"),
        };
        let plan = Plan {
            launcher,
            language: "auto".into(),
            start_service: true,
            background: true,
            progress: false,
        };
        let command = plan.command().unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|v| v.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|v| v == ["--language", "auto"]));
        assert!(args.contains(&"--start-service".into()));
        assert!(args.contains(&"--background".into()));
        assert!(args.contains(&"/path with spaces/MyGdrive.AppImage".into()));
        assert!(!args.contains(&"--snapshot".into()));
        assert!(!args.contains(&"--progress".into()));
    }
    #[test]
    fn appimage_path_filter_removes_only_old_mount_entries() {
        assert_eq!(
            without_app_dir(
                std::ffi::OsStr::new("/tmp/old/usr/lib:/custom/lib:/tmp/older/lib:/usr/lib"),
                Path::new("/tmp/old")
            )
            .unwrap(),
            "/custom/lib:/tmp/older/lib:/usr/lib"
        );
    }
}
