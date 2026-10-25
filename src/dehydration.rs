//! Local-only removal with rollback before any scanner can infer a deletion.
use crate::{db::Database, local, model::LocalFile, Error, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct Journal {
    file: LocalFile,
    staging: String,
    #[serde(default)]
    rollback: Option<String>,
    #[serde(default)]
    recovery_index: usize,
}

pub(crate) fn rename(source: &Path, target: &Path) -> Result<()> {
    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| Error::Changed)?;
    let target = CString::new(target.as_os_str().as_bytes()).map_err(|_| Error::Changed)?;
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

async fn verify_root(db: &Database, root: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(root)?;
    if !meta.is_dir()
        || db.setting("root_identity").await?.as_deref()
            != Some(&format!("{}:{}", meta.dev(), meta.ino()))
    {
        return Err(Error::Changed);
    }
    Ok(())
}

fn paths(root: &Path, journal: &Journal) -> Result<(PathBuf, PathBuf)> {
    let target = local::safe_path(root, &journal.file.relative_path)?;
    let staging = local::safe_path(root, &journal.staging)?;
    let name = staging
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or(Error::Changed)?;
    let id = name
        .strip_prefix(local::INTERNAL_PREFIX)
        .and_then(|s| s.strip_suffix(".evict"))
        .ok_or(Error::Changed)?;
    if uuid::Uuid::parse_str(id).is_err() || staging.parent() != target.parent() {
        return Err(Error::Changed);
    }
    Ok((target, staging))
}

/// Caller holds Scanner.gate. Interrupted removals are rolled back, including
/// when the metadata transition committed but the staged inode still exists.
pub async fn recover(db: &Database, root: &Path) -> Result<()> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT file_id,journal FROM dehydration_requests WHERE journal IS NOT NULL",
    )
    .fetch_all(&db.pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    verify_root(db, root).await?;
    for (id, data) in rows {
        let mut journal: Journal = serde_json::from_str(&data)?;
        if journal.file.id != id {
            return Err(Error::Changed);
        }
        let (target, staging) = paths(root, &journal)?;
        if journal.recovery_index > 1024 {
            return Err(Error::Changed);
        }
        let recovery_name = |index| {
            crate::engine::conflict_name(
                &journal.file.relative_path,
                &format!(
                    "Recovery - {}{}",
                    staging.file_stem().unwrap().to_string_lossy(),
                    if index == 0 {
                        String::new()
                    } else {
                        format!("-{index}")
                    }
                ),
            )
        };
        if journal.rollback.as_ref().is_some_and(|path| {
            path != &journal.file.relative_path && path != &recovery_name(journal.recovery_index)
        }) {
            return Err(Error::Changed);
        }
        let staged = match std::fs::symlink_metadata(&staging) {
            Ok(meta) if meta.is_file() && meta.nlink() == 1 => true,
            Ok(_) => return Err(Error::Changed),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        if staged {
            let mut destination = journal
                .rollback
                .clone()
                .unwrap_or_else(|| journal.file.relative_path.clone());
            if local::safe_path(root, &destination)?.try_exists()? {
                destination = recovery_name(journal.recovery_index);
                while local::safe_path(root, &destination)?.try_exists()? {
                    journal.recovery_index += 1;
                    if journal.recovery_index > 1024 {
                        return Err(Error::Changed);
                    }
                    destination = recovery_name(journal.recovery_index);
                }
            }
            journal.rollback = Some(destination);
            // Recovery itself may be interrupted. Remember the destination
            // before moving, so its review flag is restored before scanning.
            sqlx::query("UPDATE dehydration_requests SET journal=? WHERE file_id=?")
                .bind(serde_json::to_string(&journal)?)
                .bind(&id)
                .execute(&db.pool)
                .await?;
        }
        if let Some(relative) = &journal.rollback {
            let destination = local::safe_path(root, relative)?;
            if staged {
                rename(&staging, &destination)?;
                local::sync_directory(destination.parent().ok_or(Error::Changed)?)?;
            }
            db.record_local(relative, Some(local::fingerprint(&destination)?))
                .await?;
            if destination != target {
                let data: String =
                    sqlx::query_scalar("SELECT data FROM files WHERE relative_path=?")
                        .bind(relative)
                        .fetch_one(&db.pool)
                        .await?;
                let mut recovered: LocalFile = serde_json::from_str(&data)?;
                recovered.review_required = true;
                recovered.status = "recovery_review".into();
                db.save_file(&recovered).await?;
            }
        } else {
            let file = db.file(&id).await?;
            if !target.try_exists()? && file.materialized {
                return Err(Error::Changed);
            }
        }
        sqlx::query("UPDATE dehydration_requests SET journal=NULL WHERE file_id=?")
            .bind(id)
            .execute(&db.pool)
            .await?;
    }
    Ok(())
}

fn gone(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

/// Check after quarantining the pathname, so ordinary backing opens cannot
/// acquire the old inode between this check and unlink. Fail closed if a
/// same-user process cannot be inspected. Mappings outlive their original FD.
fn in_use(file: &File) -> Result<bool> {
    // A kernel write lease checks all opens and mappings, including processes
    // hidden by ptrace restrictions. Probe only after quarantining the path.
    // F_SETOWN_EX must precede F_SETLEASE: lease_setup preserves this owner.
    // Direct SIGIO to this thread and block/drain it during the synchronous
    // probe, without changing any process-wide signal handler or awaiting.
    struct SignalMask {
        previous: libc::sigset_t,
        blocked: libc::sigset_t,
        pending_before: bool,
    }
    impl Drop for SignalMask {
        fn drop(&mut self) {
            unsafe {
                if !self.pending_before {
                    let timeout = libc::timespec {
                        tv_sec: 0,
                        tv_nsec: 0,
                    };
                    while libc::sigtimedwait(&self.blocked, std::ptr::null_mut(), &timeout) >= 0 {}
                }
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
            }
        }
    }
    #[repr(C)]
    struct Owner {
        kind: libc::c_int,
        pid: libc::pid_t,
    }
    let mut blocked = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    let mut previous = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    unsafe {
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGIO);
    }
    let error = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous) };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error).into());
    }
    let mut pending = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    let pending_before = unsafe {
        libc::sigpending(&mut pending) != 0 || libc::sigismember(&pending, libc::SIGIO) == 1
    };
    let _mask = SignalMask {
        previous,
        blocked,
        pending_before,
    };
    let owner = Owner {
        // Linux UAPI enum f_owner_ex / F_SETOWN_EX; libc does not export
        // these constants on every supported target.
        kind: 0,
        pid: unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t },
    };
    if unsafe { libc::fcntl(file.as_raw_fd(), 15, &owner) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_WRLCK) } == -1 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::EAGAIN) => Ok(true),
            Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP | libc::ENOLCK | libc::EPERM) => {
                proc_in_use(file)
            }
            _ => Err(error.into()),
        };
    }
    let lease = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLEASE) };
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(lease != libc::F_WRLCK)
}

fn proc_in_use(file: &File) -> Result<bool> {
    // Flatpak's /proc omits host editors. If the kernel lease probe is not
    // supported, retain the bytes rather than infer that the file is unused.
    if std::env::var_os("FLATPAK_ID").is_some() || Path::new("/.flatpak-info").exists() {
        return Ok(true);
    }
    let expected = file.metadata()?;
    let current_pid = std::process::id().to_string();
    let own_fd = file.as_raw_fd().to_string();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let pid = entry.file_name();
        if !pid.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let status = match std::fs::read_to_string(entry.path().join("status")) {
            Ok(status) => status,
            Err(e) if gone(&e) => continue,
            Err(e) => return Err(e.into()),
        };
        let uid_line = status
            .lines()
            .find(|line| line.starts_with("Uid:"))
            .ok_or(Error::Changed)?;
        let uids = uid_line
            .split_whitespace()
            .skip(1)
            .map(str::parse::<u32>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| Error::Changed)?;
        if !uids.contains(&unsafe { libc::geteuid() }) {
            continue;
        }
        let fds = match std::fs::read_dir(entry.path().join("fd")) {
            Ok(fds) => fds,
            Err(e) if gone(&e) => continue,
            Err(e) => return Err(e.into()),
        };
        for fd in fds {
            let fd = match fd {
                Ok(fd) => fd,
                Err(e) if gone(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            if pid == current_pid.as_str() && fd.file_name() == own_fd.as_str() {
                continue;
            }
            match std::fs::metadata(fd.path()) {
                Ok(meta) if meta.dev() == expected.dev() && meta.ino() == expected.ino() => {
                    return Ok(true)
                }
                Ok(_) => (),
                Err(e) if gone(&e) => (),
                Err(e) => return Err(e.into()),
            }
        }
        let maps = match std::fs::read_to_string(entry.path().join("maps")) {
            Ok(maps) => maps,
            Err(e) if gone(&e) => continue,
            Err(e) => return Err(e.into()),
        };
        for line in maps.lines() {
            let mut fields = line.split_whitespace();
            let dev = fields.nth(3).ok_or(Error::Changed)?;
            let inode = fields
                .next()
                .ok_or(Error::Changed)?
                .parse::<u64>()
                .map_err(|_| Error::Changed)?;
            let (major, minor) = dev.split_once(':').ok_or(Error::Changed)?;
            if inode == expected.ino()
                && u64::from_str_radix(major, 16).map_err(|_| Error::Changed)?
                    == libc::major(expected.dev()) as u64
                && u64::from_str_radix(minor, 16).map_err(|_| Error::Changed)?
                    == libc::minor(expected.dev()) as u64
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Caller serializes lifecycle, file, scanner and pin policy. The engine has
/// freshly verified the cloud copy. Never removes directories or remote data.
pub async fn evict(db: &Database, root: &Path, file: &LocalFile) -> Result<bool> {
    verify_root(db, root).await?;
    let target = local::safe_path(root, &file.relative_path)?;
    let handle = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&target)
    {
        Ok(handle) => handle,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !file.materialized => {
            return Ok(true)
        }
        Err(e) => return Err(e.into()),
    };
    if handle.try_lock_exclusive().is_err() {
        return Ok(false);
    }
    let before = local::fingerprint(&target)?;
    let meta = handle.metadata()?;
    if meta.nlink() != 1
        || before.device != meta.dev()
        || before.inode != meta.ino()
        || file.fingerprint.as_ref() != Some(&before)
        || file.baseline.as_ref().map(|b| &b.digest) != Some(&before.digest)
    {
        return Ok(false);
    }
    let staging = Path::new(&file.relative_path).with_file_name(format!(
        "{}{}.evict",
        local::INTERNAL_PREFIX,
        uuid::Uuid::new_v4()
    ));
    let journal = Journal {
        file: file.clone(),
        staging: staging.to_str().ok_or(Error::Changed)?.into(),
        rollback: None,
        recovery_index: 0,
    };
    sqlx::query("UPDATE dehydration_requests SET journal=? WHERE file_id=?")
        .bind(serde_json::to_string(&journal)?)
        .bind(&file.id)
        .execute(&db.pool)
        .await?;
    let (_, staging) = paths(root, &journal)?;
    rename(&target, &staging)?;
    local::sync_directory(target.parent().ok_or(Error::Changed)?)?;
    let idle = in_use(&handle);
    // A backing editor may write and close while /proc is being inspected.
    // Hash afterward so its final writes cannot be discarded.
    let fp = local::fingerprint(&staging)?;
    if fp.device != before.device
        || fp.inode != before.inode
        || fp.digest != before.digest
        || !matches!(idle, Ok(false))
    {
        recover(db, root).await?;
        return match idle {
            Err(e) => Err(e),
            _ => Ok(false),
        };
    }
    let mut online = db.file(&file.id).await?;
    if online.generation != file.generation || online.relative_path != file.relative_path {
        recover(db, root).await?;
        return Ok(false);
    }
    online.generation += 1;
    online.fingerprint = None;
    online.materialized = false;
    online.available_offline = false;
    online.status = "online_only".into();
    db.save_file(&online).await?;
    std::fs::remove_file(&staging)?;
    local::sync_directory(target.parent().ok_or(Error::Changed)?)?;
    sqlx::query("UPDATE dehydration_requests SET journal=NULL WHERE file_id=?")
        .bind(&file.id)
        .execute(&db.pool)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;

    #[test]
    fn proc_fallback_in_a_sandbox_never_declares_host_files_unused() {
        const CHILD: &str = "MYGDRIVE_TEST_SANDBOX_LEASE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let file = tempfile::tempfile().unwrap();
            assert!(proc_in_use(&file).unwrap());
            return;
        }
        // Use a separate process: changing the environment of concurrent tests
        // could accidentally disable host-side open-file checks elsewhere.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "dehydration::sandbox_tests::proc_fallback_in_a_sandbox_never_declares_host_files_unused",
            ])
            .env("FLATPAK_ID", "io.github.mhdr1zky.MyGdrive")
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
