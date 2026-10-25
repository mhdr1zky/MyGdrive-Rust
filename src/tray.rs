//! StatusNotifierItem adapter. Callbacks send intents; GTK and IPC run elsewhere.
use crate::{
    dashboard::{Command, Snapshot},
    Result,
};
use ksni::{menu::StandardItem, Tray, TrayMethods};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub title: String,
    pub active: bool,
    pub paused: bool,
    pub configured: bool,
    pub busy: bool,
    pub syncing: bool,
    pub attention: bool,
    pub warning: bool,
}
impl Model {
    pub fn from_snapshot(s: &Snapshot, busy: bool) -> Self {
        Self {
            title: s
                .notice()
                .map(|n| n.title())
                .unwrap_or_else(|| s.title())
                .into(),
            active: s.live && s.status.as_ref().is_some_and(|s| !s.shutdown_requested),
            paused: s.status.as_ref().is_some_and(|s| s.paused),
            configured: s.configuration().is_some(),
            busy,
            syncing: s.notice().is_none()
                && s.live
                && s.status.as_ref().is_some_and(|s| {
                    s.running
                        && !s.paused
                        && !s.shutdown_requested
                        && s.progress.has_file_activity(crate::now_ms())
                }),
            attention: s.notice().is_some_and(|n| n.is_error()),
            warning: s.notice().is_some_and(|n| !n.is_error()),
        }
    }
    /// Use the same complete queue count as the activity popup. Service history
    /// counters can include tombstones; they must not keep the badge spinning.
    pub fn with_pending_files(mut self, pending_files: usize) -> Self {
        if pending_files > 0 && self.active && !self.paused && !self.attention && !self.warning {
            self.syncing = true;
            self.title = format!("Menyinkronkan {pending_files} berkas");
        }
        self
    }
}
#[derive(Debug, Clone, Copy)]
pub enum Event {
    Show,
    Progress,
    OpenFolder,
    Control(Command),
    Quit,
}
struct Item {
    model: Model,
    events: mpsc::SyncSender<Event>,
    online: Arc<AtomicBool>,
    id: String,
}
impl Item {
    fn item(&self, label: &str, enabled: bool, event: Event) -> ksni::MenuItem<Self> {
        StandardItem {
            label: label.into(),
            enabled,
            activate: Box::new(move |s: &mut Self| {
                let _ = s.events.try_send(event);
            }),
            ..Default::default()
        }
        .into()
    }
}
impl Tray for Item {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn title(&self) -> String {
        format!("MyGdrive Rust · {}", self.model.title)
    }
    fn status(&self) -> ksni::Status {
        ksni::Status::Active
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.events.try_send(Event::Progress);
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let badge = if self.model.attention {
            [220, 75, 65]
        } else if self.model.paused || self.model.warning {
            [238, 175, 53]
        } else {
            [50, 188, 131]
        };
        let mut data = vec![0; 32 * 32 * 4];
        for y in 0..32_i32 {
            for x in 0..32_i32 {
                let circle = |cx: i32, cy: i32, r: i32| (x - cx).pow(2) + (y - cy).pow(2) <= r * r;
                // Two opposing curved arrows replace the idle dot during a live sync cycle.
                let dx = x - 25;
                let dy = y - 25;
                let radius_squared = dx * dx + dy * dy;
                let sync_arrow = (9..=25).contains(&radius_squared) && dy.abs() >= 2
                    || ((2..=5).contains(&dx) && (-3..=1).contains(&dy) && dy >= dx - 5)
                    || ((-5..=-2).contains(&dx) && (-1..=3).contains(&dy) && dy <= dx + 5);
                let color = if self.model.syncing && circle(25, 25, 6) {
                    Some(if sync_arrow {
                        [52, 107, 232]
                    } else {
                        [255, 255, 255]
                    })
                } else if circle(25, 25, 6) {
                    Some(badge)
                } else if circle(25, 25, 8)
                    || circle(11, 15, 5)
                    || circle(17, 12, 7)
                    || circle(23, 16, 5)
                    || ((8..=25).contains(&x) && (15..=21).contains(&y))
                {
                    Some([255, 255, 255])
                } else if circle(16, 16, 15) {
                    Some([52, 107, 232])
                } else {
                    None
                };
                if let Some(rgb) = color {
                    let i = (y as usize * 32 + x as usize) * 4;
                    data[i..i + 4].copy_from_slice(&[255, rgb[0], rgb[1], rgb[2]]);
                }
            }
        }
        vec![ksni::Icon {
            width: 32,
            height: 32,
            data,
        }]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "MyGdrive Rust".into(),
            description: self.model.title.clone(),
            ..Default::default()
        }
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let m = &self.model;
        vec![
            StandardItem {
                label: m.title.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            self.item("Tampilkan MyGdrive Rust", true, Event::Show),
            self.item("Buka folder lokal", m.configured, Event::OpenFolder),
            self.item(
                if m.paused {
                    "Lanjutkan sinkronisasi"
                } else {
                    "Jeda sinkronisasi"
                },
                m.active && !m.busy,
                Event::Control(if m.paused {
                    Command::Resume
                } else {
                    Command::Pause
                }),
            ),
            self.item(
                "Sinkronkan sekarang",
                m.active && !m.paused && !m.busy,
                Event::Control(Command::SyncNow),
            ),
            self.item(
                "Mulai service",
                !m.active && m.configured && !m.busy,
                Event::Control(Command::Start),
            ),
            self.item("Aktivitas sinkronisasi", true, Event::Progress),
            ksni::MenuItem::Separator,
            self.item("Keluar dan hentikan sinkronisasi", !m.busy, Event::Quit),
        ]
    }
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        self.online.store(false, Ordering::Release);
        true
    }
    fn watcher_online(&self) {
        self.online.store(true, Ordering::Release);
    }
}
pub struct Controller {
    model: tokio::sync::watch::Sender<Model>,
    pub events: mpsc::Receiver<Event>,
    online: Arc<AtomicBool>,
    stop: CancellationToken,
}
impl Controller {
    pub fn update(&self, model: Model) {
        self.model.send_if_modified(|old| {
            if *old == model {
                false
            } else {
                *old = model;
                true
            }
        });
    }
    pub fn available(&self) -> bool {
        self.online.load(Ordering::Acquire)
    }
    pub fn stop(&self) {
        self.stop.cancel();
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.stop();
    }
}
async fn run_worker(
    mut input: tokio::sync::watch::Receiver<Model>,
    events: mpsc::SyncSender<Event>,
    online: Arc<AtomicBool>,
    cancel: CancellationToken,
    id: String,
) {
    while !cancel.is_cancelled() {
        let model = input.borrow_and_update().clone();
        let item = Item {
            model,
            events: events.clone(),
            online: online.clone(),
            id: id.clone(),
        };
        let result = tokio::select! {
            _ = cancel.cancelled() => break,
            result = tokio::time::timeout(std::time::Duration::from_secs(10), item.disable_dbus_name(std::env::var_os("FLATPAK_ID").is_some()).spawn()) => result,
        };
        if let Ok(Ok(handle)) = result {
            online.store(true, Ordering::Release);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    changed = input.changed() => {
                        if changed.is_err() { break; }
                        let model = input.borrow_and_update().clone();
                        let result = tokio::select! {
                            _ = cancel.cancelled() => break,
                            result = handle.update(|item| item.model = model) => result,
                        };
                        if result.is_none() { break; }
                    },
                }
            }
            online.store(false, Ordering::Release);
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(2), handle.shutdown()).await;
        }
        // The panel may start after XDG autostart, or the session bus may reconnect.
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {},
        }
    }
    online.store(false, Ordering::Release);
}
pub fn spawn(state: &std::path::Path, model: Model) -> Result<Controller> {
    let (sender, input) = tokio::sync::watch::channel(model);
    let (events, receiver) = mpsc::sync_channel(32);
    let stop = CancellationToken::new();
    let cancel = stop.clone();
    let online = Arc::new(AtomicBool::new(false));
    let worker_online = online.clone();
    let id = format!("mygdrive-{}", crate::autostart::profile_id(state));
    std::thread::Builder::new()
        .name("mygdrive-tray".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(run_worker(input, events, worker_online, cancel, id));
        })?;
    Ok(Controller {
        model: sender,
        events: receiver,
        online,
        stop,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queued_files_keep_badge_active_until_drained_and_preserve_warning_states() {
        let idle = Model {
            title: "Semua tersinkron".into(),
            active: true,
            paused: false,
            configured: true,
            busy: false,
            syncing: false,
            attention: false,
            warning: false,
        };
        for remaining in [45, 2, 1] {
            let queued = idle.clone().with_pending_files(remaining);
            assert!(queued.syncing, "waiting files must keep the arrows visible");
            assert!(queued.title.contains(&remaining.to_string()));
        }
        assert!(!idle.clone().with_pending_files(0).syncing);
        let mut paused = idle.clone();
        paused.paused = true;
        let mut offline = idle.clone();
        offline.warning = true;
        let mut error = idle.clone();
        error.attention = true;
        let mut stopped = idle.clone();
        stopped.active = false;
        for state in [paused, offline, error, stopped] {
            assert!(!state.with_pending_files(45).syncing);
        }
        let mut transferring = idle;
        transferring.syncing = true;
        assert!(transferring.with_pending_files(0).syncing);
    }
    #[test]
    fn routine_checks_and_completed_transfers_return_to_idle_badge() {
        let mut progress = crate::engine::SyncProgress {
            stage: "reconciling".into(),
            ..Default::default()
        };
        assert!(!progress.has_file_activity(10_000));
        progress.active_operations = 1;
        assert!(progress.has_file_activity(10_000));
        progress.active_operations = 0;
        progress.last_activity_ms = 10_000;
        assert!(progress.has_file_activity(10_500));
        assert!(!progress.has_file_activity(12_500));
        // More polling must not revive the transfer indicator.
        progress.stage = "change_feed".into();
        assert!(!progress.has_file_activity(15_000));
    }
    #[test]
    fn inactive_history_cannot_enable_remote_actions_and_callbacks_only_queue_intents() {
        let model = Model::from_snapshot(
            &Snapshot {
                live: false,
                status: None,
                config: None,
                account: None,
            },
            false,
        );
        let (events, rx) = mpsc::sync_channel(8);
        let mut item = Item {
            model,
            events,
            online: Arc::new(AtomicBool::new(false)),
            id: "test".into(),
        };
        let mut menu = item.menu();
        let ksni::MenuItem::Standard(pause) = &menu[4] else {
            panic!("pause item");
        };
        assert!(!pause.enabled);
        let ksni::MenuItem::Standard(start) = &menu[6] else {
            panic!("start item");
        };
        assert!(!start.enabled);
        let ksni::MenuItem::Standard(show) = menu.remove(2) else {
            panic!("show item");
        };
        (show.activate)(&mut item);
        assert!(matches!(rx.try_recv().unwrap(), Event::Show));
        assert_eq!(item.icon_pixmap()[0].data.len(), 32 * 32 * 4);
    }
}
