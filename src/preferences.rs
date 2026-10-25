//! Per-user desktop preferences, separate from account state and OAuth data.
use crate::{Error, Result};
use fs2::FileExt;
use serde_json::{Map, Value};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

fn valid_language(value: &str) -> bool {
    value == "auto" || crate::i18n::language_code(value).as_deref() == Some(value)
}

pub fn directory() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|base| base.config_dir().join("mygdrive"))
}
fn read(directory: &Path) -> Result<Map<String, Value>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("desktop-settings.json"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() || file.metadata()?.len() > 16384 {
        return Err(Error::Invalid("invalid desktop settings file".into()));
    }
    let mut text = String::new();
    file.take(16385).read_to_string(&mut text)?;
    Ok(serde_json::from_str(&text)?)
}
pub fn language(directory: &Path) -> Result<Option<String>> {
    Ok(read(directory)?
        .get("language")
        .and_then(Value::as_str)
        .filter(|value| valid_language(value))
        .map(str::to_owned))
}
pub fn save_language(directory: &Path, language: &str) -> Result<()> {
    if !valid_language(language) {
        return Err(Error::Invalid("unsupported interface language".into()));
    }
    crate::local::private_dir(directory)?;
    if fs::symlink_metadata(directory)?.file_type().is_symlink() {
        return Err(Error::Invalid(
            "desktop settings directory is a symlink".into(),
        ));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join(".desktop-settings.lock"))?;
    lock.lock_exclusive()?;
    let result = (|| {
        let mut settings = read(directory)?;
        settings.insert("language".into(), language.into());
        let path = directory.join(format!(".desktop-settings-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(&serde_json::to_vec_pretty(&settings)?)?;
            file.sync_all()?;
            fs::rename(&path, directory.join("desktop-settings.json"))?;
            fs::File::open(directory)?.sync_all()?;
            Ok(())
        })();
        let _ = fs::remove_file(path);
        result
    })();
    let _ = FileExt::unlock(&lock);
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    #[test]
    fn saves_all_choices_privately_and_preserves_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(language(dir.path()).unwrap(), None);
        fs::write(dir.path().join("desktop-settings.json"), r#"{"other":42}"#).unwrap();
        for selected in ["auto", "en", "id", "zh", "fr", "ja"] {
            save_language(dir.path(), selected).unwrap();
            assert_eq!(language(dir.path()).unwrap().as_deref(), Some(selected));
            assert_eq!(read(dir.path()).unwrap()["other"], 42);
            assert_eq!(
                fs::metadata(dir.path().join("desktop-settings.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(save_language(dir.path(), "../invalid").is_err());
    }
    #[test]
    fn malformed_or_symlink_settings_are_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop-settings.json");
        fs::write(&path, "invalid JSON").unwrap();
        assert!(save_language(dir.path(), "en").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "invalid JSON");
        fs::remove_file(&path).unwrap();
        let other = dir.path().join("other");
        fs::write(&other, "{}").unwrap();
        symlink(&other, &path).unwrap();
        assert!(save_language(dir.path(), "id").is_err());
        assert_eq!(fs::read_to_string(other).unwrap(), "{}");
    }
}
