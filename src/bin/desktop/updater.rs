use gtk::{glib, prelude::*};
use mygdrive_sync::i18n::{fmt, tr};
use sha2::{Digest, Sha256};
use std::{cell::Cell, path::PathBuf, process::Command, rc::Rc, sync::mpsc, time::Duration};

const RELEASES: &str = "https://github.com/mhdr1zky/MyGdrive-Rust/releases/latest";

fn installation() -> Option<(&'static str, PathBuf)> {
    let executable = std::env::current_exe().ok()?;
    if let (Some(image), Some(app_dir)) = (std::env::var_os("APPIMAGE"), std::env::var_os("APPDIR"))
    {
        if executable.starts_with(PathBuf::from(app_dir)) {
            return Some(("appimage", PathBuf::from(image)));
        }
    }
    if executable == std::path::Path::new("/usr/lib/mygdrive-rust/bin/mygdrive-desktop") {
        Some(("deb", PathBuf::from("/usr/lib/mygdrive-rust")))
    } else {
        Some(("source", executable))
    }
}

#[derive(Clone)]
enum Request {
    Check,
    Install(String),
}

fn status_message(status: &serde_json::Value) -> String {
    let version = status["available_version"]
        .as_str()
        .or_else(|| status["current_version"].as_str())
        .unwrap_or("—");
    let key = match status["phase"].as_str() {
        Some("checking") => "Memeriksa rilis GitHub…",
        Some("current") => "Version {0} is up to date.",
        Some("available") => "Version {0} is available. You choose when to update.",
        Some("manual") => "Version {0} is available. Choose a package from the release page.",
        Some("downloading") => "Downloading update {0}…",
        Some("installing") => "Installing update {0}…",
        Some("installed") => {
            "Update installed. Finishing synchronization before restarting automatically…"
        }
        Some("error") => "Update failed. Check the connection and permissions, then retry.",
        _ => "Status pembaruan belum tersedia.",
    };
    fmt(key, &[("0", version.to_owned())])
}

fn version(value: &str) -> Option<[u64; 3]> {
    let parts: Vec<_> = value.trim_start_matches('v').split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some([
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ])
}
fn restart_ready(
    kind: &str,
    status: &serde_json::Map<String, serde_json::Value>,
    succeeded: bool,
) -> bool {
    if !succeeded
        || !matches!(kind, "deb" | "appimage")
        || status.get("phase").and_then(|v| v.as_str()) != Some("installed")
    {
        return false;
    }
    let running = version(env!("CARGO_PKG_VERSION")).expect("package semantic version");
    ["installed_version", "current_version"].iter().any(|key| {
        status
            .get(*key)
            .and_then(|v| v.as_str())
            .and_then(version)
            .is_some_and(|v| v > running)
    })
}

/// Network, status files and package operations run outside the GTK main thread.
/// The periodic path can only check; installation requires a button request.
pub fn panel(
    snapshot_mode: bool,
    updating: Rc<Cell<bool>>,
    restart: impl Fn() + 'static,
) -> gtk::Box {
    let restart = Rc::new(restart);
    let panel = super::card();
    panel.append(&super::label(tr("APLIKASI"), "eyebrow"));
    panel.append(&super::label(
        &format!("MyGdrive Rust  ·  v{}", env!("CARGO_PKG_VERSION")),
        "detail",
    ));
    let message = super::label(tr("Memeriksa rilis GitHub…"), "subtitle");
    panel.append(&message);
    let actions = super::flow();
    let check = gtk::Button::with_label(tr("Periksa pembaruan"));
    let install = gtk::Button::with_label(tr("Update sekarang"));
    install.add_css_class("suggested-action");
    install.set_visible(false);
    let restart_button = gtk::Button::with_label(tr("Restart now"));
    restart_button.set_visible(false);
    let retry_restart = restart.clone();
    restart_button.connect_clicked(move |_| retry_restart());
    actions.insert(&restart_button, -1);
    if let Some(child) = restart_button.parent() {
        child.set_visible(false);
    }
    let release = gtk::LinkButton::with_label(RELEASES, tr("Catatan rilis ↗"));
    actions.insert(&check, -1);
    actions.insert(&install, -1);
    if let Some(child) = install.parent() {
        child.set_visible(false);
    }
    actions.insert(&release, -1);
    panel.append(&actions);
    panel.append(&super::label(
        tr("Pemasangan dimulai hanya saat Anda memilih Update sekarang."),
        "subtitle",
    ));
    if snapshot_mode {
        message.set_text(tr("Status rilis akan diperiksa saat aplikasi dibuka."));
        check.set_sensitive(false);
        release.set_sensitive(false);
        return panel;
    }
    if std::env::var_os("MYGDRIVE_DISABLE_AUTO_UPDATE").is_some() {
        message.set_text(tr(
            "Pemeriksaan pembaruan dinonaktifkan melalui pengaturan lingkungan.",
        ));
        check.set_sensitive(false);
        return panel;
    }
    let Some((kind, target)) = installation() else {
        return panel;
    };
    let Some(base) = directories::BaseDirs::new() else {
        return panel;
    };
    let digest = format!(
        "{:x}",
        Sha256::digest(format!("{kind}:{}", target.display()).as_bytes())
    );
    let path = base
        .data_local_dir()
        .join("mygdrive/updates")
        .join(&digest[..16])
        .join("status.json");
    let (requests, receiver) = mpsc::sync_channel(1);
    let (events, results) = mpsc::channel::<serde_json::Value>();
    check.set_sensitive(false);
    updating.set(true);
    let worker = std::thread::Builder::new()
        .name("github-updater".into())
        .spawn(move || {
            let mut request = None;
            loop {
                let mut command = Command::new("python3");
                command
                    .args(["-c", include_str!("../../../scripts/auto-update.py")])
                    .args(["--kind", kind, "--version", env!("CARGO_PKG_VERSION")])
                    .arg("--target")
                    .arg(&target)
                    .arg("--status")
                    .arg(&path);
                match &request {
                    Some(Request::Install(version)) => {
                        command.args(["--install", "--expected-version", version]);
                    }
                    Some(Request::Check) => {
                        command.arg("--force");
                    }
                    None => {}
                }
                let mut failed = None;
                let mut reported = false;
                let mut succeeded = false;
                match command.spawn() {
                    Ok(mut child) => loop {
                        let done = match child.try_wait() {
                            Ok(Some(status)) => {
                                succeeded = status.success();
                                if !status.success() {
                                    failed = Some(tr("Pemeriksaan gagal. Pastikan Python 3 tersedia."));
                                }
                                true
                            }
                            Ok(None) => false,
                            Err(_) => {
                                failed = Some(tr("Status proses pembaruan tidak dapat dibaca."));
                                true
                            }
                        };
                        if let Ok(text) = std::fs::read_to_string(&path) {
                            if let Ok(mut status) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
                            {
                                reported = true;
                                status.insert("busy".into(), (!done).into());
                                let ready = restart_ready(kind, &status, done && succeeded);
                                status.insert("restart_ready".into(), ready.into());
                                if events.send(serde_json::Value::Object(status)).is_err() {
                                    return;
                                }
                            }
                        }
                        if done {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    },
                    Err(_) => {
                        failed = Some(tr("Updater tidak dapat dimulai. Pastikan Python 3 tersedia."))
                    }
                }
                if !reported && failed.is_none() {
                    failed = Some(tr("Status pembaruan belum tersedia. Tekan Periksa pembaruan untuk mencoba lagi."));
                }
                if let Some(error) = failed {
                    if events
                        .send(serde_json::json!({"phase":"error", "message":error, "localized":true}))
                        .is_err()
                    {
                        return;
                    }
                }
                request = match receiver.recv_timeout(Duration::from_secs(15 * 60)) {
                    Ok(request) => Some(request),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                };
            }
        });
    if worker.is_err() {
        updating.set(false);
        message.set_text(tr(
            "Worker pembaruan tidak dapat dimulai. Buka ulang aplikasi.",
        ));
        return panel;
    }
    let selected = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let commands = requests.clone();
    let pending_check = check.clone();
    let pending_install = install.clone();
    let feedback = message.clone();
    let checking = updating.clone();
    check.connect_clicked(move |_| {
        if commands.try_send(Request::Check).is_ok() {
            checking.set(true);
            pending_check.set_sensitive(false);
            pending_install.set_sensitive(false);
            feedback.set_text(tr("Memeriksa rilis GitHub…"));
        }
    });
    let manual = std::rc::Rc::new(std::cell::Cell::new(false));
    let manual_action = manual.clone();
    let version = selected.clone();
    let pending_check = check.clone();
    let feedback = message.clone();
    let installing = updating.clone();
    install.connect_clicked(move |button| {
        if manual_action.get() {
            super::launch_uri(RELEASES, &feedback);
            return;
        }
        if let Some(version) = version.borrow().as_ref() {
            if requests.try_send(Request::Install(version.clone())).is_ok() {
                installing.set(true);
                button.set_sensitive(false);
                pending_check.set_sensitive(false);
                feedback.set_text(tr("Menyiapkan pembaruan yang Anda pilih…"));
            }
        }
    });
    let automatically_restarted = Cell::new(false);
    let weak = panel.downgrade();
    glib::timeout_add_local(Duration::from_millis(250), move || {
        if weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        while let Ok(status) = results.try_recv() {
            // Locally generated worker errors are already translated. Python status
            // messages are diagnostics; phase and version drive the visible text.
            if status["localized"].as_bool() == Some(true) {
                message.set_text(status["message"].as_str().unwrap_or(""));
            } else {
                message.set_text(&status_message(&status));
            }
            message.set_tooltip_text(status["message"].as_str());
            let busy = status["busy"].as_bool().unwrap_or(false);
            updating.set(busy);
            let ready = status["restart_ready"].as_bool().unwrap_or(false);
            restart_button.set_visible(ready);
            if let Some(child) = restart_button.parent() {
                child.set_visible(ready);
            }
            if ready && !automatically_restarted.replace(true) {
                restart();
            }
            let is_manual = status["phase"] == "manual";
            manual.set(is_manual);
            install.set_tooltip_text(Some(if is_manual { tr("Buka rilis GitHub untuk memilih paket instalasi.") } else { tr("Unduh, verifikasi, lalu pasang versi yang tersedia. Paket .deb meminta izin administrator.") }));
            let available = status["phase"] == "available" || is_manual;
            *selected.borrow_mut() = status["available_version"]
                .as_str()
                .filter(|_| available)
                .map(str::to_owned);
            install.set_visible(available);
            if let Some(child) = install.parent() {
                child.set_visible(available);
            }
            install.set_sensitive(available && !busy);
            check.set_sensitive(!busy);
        }
        glib::ControlFlow::Continue
    });
    panel
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restarts_only_after_confirmed_newer_packaged_installation() {
        let mut status = serde_json::json!({"phase":"installed", "installed_version":"999.0.0"})
            .as_object()
            .unwrap()
            .clone();
        assert!(restart_ready("deb", &status, true));
        assert!(restart_ready("appimage", &status, true));
        assert!(!restart_ready("source", &status, true));
        assert!(!restart_ready("deb", &status, false));
        for phase in [
            "checking",
            "available",
            "downloading",
            "installing",
            "error",
        ] {
            status.insert("phase".into(), phase.into());
            assert!(!restart_ready("deb", &status, true));
        }
        status.insert("phase".into(), "installed".into());
        for value in [
            env!("CARGO_PKG_VERSION"),
            "0.0.1",
            "invalid",
            "999.0.0-beta",
        ] {
            status.insert("installed_version".into(), value.into());
            assert!(!restart_ready("deb", &status, true));
        }
        status.insert("current_version".into(), "999.0.0".into());
        assert!(restart_ready("deb", &status, true));
    }
}
