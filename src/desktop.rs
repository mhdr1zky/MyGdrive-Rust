//! Local desktop integration; independent of OAuth and the sync writer lock.
use crate::{Error, Result};
use fs2::FileExt;
use reqwest::Url;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

/// Append a GTK3/Thunar shortcut while preserving existing bookmarks and labels.
/// Repeated installation leaves an existing shortcut to this directory untouched.
pub fn install_thunar_shortcut(root: &Path, label: &str, config_dir: &Path) -> Result<PathBuf> {
    if label.trim().is_empty() || label.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "shortcut label must be nonempty and contain no control characters".into(),
        ));
    }
    let root = root.canonicalize()?;
    if !root.is_dir() {
        return Err(Error::Invalid("shortcut target must be a directory".into()));
    }
    let uri = Url::from_file_path(&root)
        .map_err(|_| Error::Invalid("cannot encode local folder URI".into()))?;
    let parent = config_dir.join("gtk-3.0");
    fs::create_dir_all(&parent)?;
    let path = parent.join("bookmarks");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > 1024 * 1024 {
        return Err(Error::Invalid(
            "GTK bookmarks must be a regular file smaller than 1 MiB".into(),
        ));
    }
    file.try_lock_exclusive()
        .map_err(|_| Error::Invalid("bookmarks are being updated; retry later".into()))?;
    let mut contents = String::new();
    std::io::Read::by_ref(&mut file)
        .take(1024 * 1024 + 1)
        .read_to_string(&mut contents)?;
    if contents.len() > 1024 * 1024 {
        return Err(Error::Changed);
    }
    if contents.lines().any(|line| {
        line.split_whitespace()
            .next()
            .and_then(|v| Url::parse(v).ok())
            .and_then(|u| u.to_file_path().ok())
            .is_some_and(|p| p == root)
    }) {
        return Ok(path);
    }
    let separator = if contents.is_empty() || contents.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let entry = format!("{separator}{uri} {label}\n");
    // Append in one write: existing entries are never rewritten or truncated.
    // Detect an external update before appending; GTK does not share our advisory lock.
    if file.seek(SeekFrom::End(0))? != contents.len() as u64 {
        return Err(Error::Changed);
    }
    file.write_all(entry.as_bytes())?;
    file.sync_all()?;
    fs::File::open(parent)?.sync_all()?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_existing_labels_and_encodes_special_paths_without_duplicates() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("My Drive #1");
        fs::create_dir(&root).unwrap();
        let config = temp.path().join("config");
        fs::create_dir_all(config.join("gtk-3.0")).unwrap();
        let bookmarks = config.join("gtk-3.0/bookmarks");
        let previous = "file:///home/example/other My other folder";
        fs::write(&bookmarks, previous).unwrap();
        install_thunar_shortcut(&root, "MyGDrive", &config).unwrap();
        let installed = fs::read_to_string(&bookmarks).unwrap();
        assert!(installed.starts_with(&format!("{previous}\n")));
        assert!(installed.contains("My%20Drive%20%231 MyGDrive\n"));
        install_thunar_shortcut(&root, "Another label", &config).unwrap();
        assert_eq!(fs::read_to_string(bookmarks).unwrap(), installed);
    }
    #[test]
    fn rejects_label_injection_and_bookmark_symlinks_without_modifying_target() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        assert!(
            install_thunar_shortcut(temp.path(), "MyGDrive\nfile:///injected", &config).is_err()
        );
        fs::create_dir_all(config.join("gtk-3.0")).unwrap();
        let target = temp.path().join("untouched");
        fs::write(&target, "existing data").unwrap();
        std::os::unix::fs::symlink(&target, config.join("gtk-3.0/bookmarks")).unwrap();
        assert!(install_thunar_shortcut(temp.path(), "MyGDrive", &config).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "existing data");
    }
}
