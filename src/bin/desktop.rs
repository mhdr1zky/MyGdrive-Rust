use clap::Parser;
use gtk::{gio, glib, prelude::*};
use mygdrive_sync::dashboard::{self, Command, Event, Snapshot};
use mygdrive_sync::i18n::{fmt, tr};
use mygdrive_sync::tray;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::PathBuf,
    rc::Rc,
    time::Duration,
};
#[path = "desktop/accounts.rs"]
mod accounts;
#[path = "desktop/maintenance.rs"]
mod maintenance;
#[path = "desktop/offline.rs"]
mod offline;
#[path = "desktop/position.rs"]
mod position;
#[path = "desktop/progress.rs"]
mod progress;
#[path = "desktop/restart.rs"]
mod restart;
#[path = "desktop/settings.rs"]
mod settings;
#[path = "desktop/setup.rs"]
mod setup;
#[path = "desktop/theme.rs"]
mod theme;
#[path = "desktop/updater.rs"]
mod updater;

#[derive(Default)]
struct RestartRequest {
    update: bool,
    language: Option<String>,
}

#[derive(Parser, Clone)]
#[command(about = "MyGdrive Rust desktop dashboard")]
struct Args {
    /// Interface language: auto (system), en, id, zh, or a custom catalog code.
    #[arg(long, env = "MYGDRIVE_LANGUAGE")]
    language: Option<String>,
    #[arg(long)]
    state: Option<PathBuf>,
    /// Render the application window to PNG and exit, for visual verification.
    #[arg(long)]
    snapshot: Option<PathBuf>,
    /// Open account/folder setup; with --snapshot, render that dialog without actions.
    #[arg(long)]
    setup: bool,
    /// Start hidden once the panel has registered the tray; fall back to a visible window.
    #[arg(long)]
    background: bool,
    /// Start the saved service, preserving its paused state. Used by opt-in autostart.
    #[arg(long)]
    start_service: bool,
    /// Open the offline file/folder browser (render only with --snapshot).
    #[arg(long)]
    offline: bool,
    /// Open the compact tray activity window.
    #[arg(long)]
    progress: bool,
    /// Open storage usage, file issues and recovery review.
    #[arg(long)]
    maintenance: bool,
    /// Open the unified account manager.
    #[arg(long)]
    accounts: bool,
    #[arg(long, hide = true)]
    restore_accounts: bool,
}
const CSS: &str = include_str!("desktop/style.css");
fn label(text: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_max_width_chars(65);
    label.add_css_class(class);
    label
}
fn row() -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, 12)
}
/// GTK FlowBox wraps actions and cards naturally at the available window width.
fn flow() -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(8)
        .row_spacing(8)
        .min_children_per_line(1)
        .max_children_per_line(4)
        .build()
}
fn column(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, spacing)
}
fn card() -> gtk::Box {
    let b = column(8);
    b.add_css_class("card");
    b
}
fn time_text(ms: Option<i64>) -> String {
    ms.and_then(|v| glib::DateTime::from_unix_local(v / 1000).ok())
        .and_then(|d| d.format("%d %b, %H:%M:%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| tr("Belum ada").into())
}
struct Widgets {
    title: gtk::Label,
    pill: gtk::Label,
    description: gtk::Label,
    profile: gtk::Label,
    folder: gtk::Label,
    remote: gtk::Label,
    pending: gtk::Label,
    operations: gtk::Label,
    success: gtk::Label,
    stage: gtk::Label,
    error: gtk::Label,
    pause: gtk::Button,
    sync: gtk::Button,
    start: gtk::Button,
    stop: gtk::Button,
    open: gtk::Button,
    drive: gtk::Button,
}
impl Widgets {
    fn render(&self, snapshot: &Snapshot, busy: bool) {
        self.title.set_text(snapshot.title());
        let status = snapshot.status.as_ref();
        let active = snapshot.live && status.is_some_and(|s| !s.shutdown_requested);
        let paused = status.is_some_and(|s| s.paused);
        let notice = snapshot.notice();
        self.pill.set_text(if !snapshot.live {
            tr("Layanan nonaktif")
        } else if paused {
            tr("Dijeda")
        } else if notice.is_some() {
            tr("Perlu perhatian")
        } else {
            tr("Terhubung")
        });
        if !snapshot.live || paused || notice.is_some() {
            self.pill.add_css_class("warning");
        } else {
            self.pill.remove_css_class("warning");
        }
        self.description.set_text(if snapshot.live {
            if let Some(notice) = notice {
                notice.description()
            } else if paused {
                tr("Perubahan lokal tetap disimpan. Lanjutkan saat Anda siap.")
            } else {
                tr("Folder Anda dipantau. Perubahan disinkronkan secara berkala.")
            }
        } else if status.is_some() {
            tr("Menampilkan catatan terakhir. Mulai layanan untuk memperbarui status.")
        } else if snapshot.configuration().is_some() {
            tr("Folder sudah dikonfigurasi. Mulai layanan untuk menyinkronkan file.")
        } else {
            tr("Hubungkan Google Drive melalui Akun dan folder untuk memulai.")
        });
        self.pause
            .set_label(if paused { tr("Lanjutkan") } else { tr("Jeda") });
        self.pause.set_sensitive(active && !busy);
        self.sync.set_sensitive(active && !paused && !busy);
        self.stop.set_sensitive(active && !busy);
        self.start
            .set_sensitive(!snapshot.live && snapshot.configuration().is_some() && !busy);
        self.start.set_visible(!snapshot.live);
        self.stop.set_visible(snapshot.live);
        self.pause.set_visible(snapshot.live);
        self.sync.set_visible(snapshot.live);
        for button in [&self.start, &self.stop, &self.pause, &self.sync] {
            if let Some(child) = button.parent() {
                child.set_visible(button.is_visible());
            }
        }
        self.open.set_sensitive(snapshot.configuration().is_some());
        self.drive.set_sensitive(snapshot.configuration().is_some());
        if let Some(s) = status {
            self.profile.set_text(
                &snapshot
                    .account
                    .as_ref()
                    .map(|a| fmt("Google Drive · {}", &[("0", (a.email).to_string())]))
                    .unwrap_or_else(|| {
                        fmt(
                            "Google Drive · Profil {}",
                            &[("0", (s.config.account).to_string())],
                        )
                    }),
            );
            self.folder.set_text(&s.config.root.to_string_lossy());
            self.remote.set_text(&fmt(
                "Interval sinkronisasi: {} detik",
                &[("0", (s.config.interval_seconds).to_string())],
            ));
            self.pending.set_text(&s.pending_files.to_string());
            self.operations.set_text(&s.pending_operations.to_string());
            self.success.set_text(&time_text(s.last_success_ms));
            let stage = if !snapshot.live {
                tr("Catatan terakhir")
            } else if s.paused {
                tr("Watcher lokal aktif")
            } else if !s.cycle_active {
                tr("Menunggu siklus berikutnya")
            } else {
                match s.progress.stage.as_str() {
                    "local_scan" => tr("Memindai perubahan lokal"),
                    "inventory" => tr("Membaca daftar file Drive"),
                    "change_feed" => tr("Memeriksa perubahan Drive"),
                    "reconciling" => tr("Menyelaraskan file"),
                    _ => tr("Menunggu siklus berikutnya"),
                }
            };
            if s.progress.inventory_pages > 0 {
                self.stage.set_text(&fmt(
                    "{stage} · {} halaman / {} item inventaris",
                    &[
                        ("0", (s.progress.inventory_pages).to_string()),
                        ("1", (s.progress.inventory_items).to_string()),
                        ("stage", stage.to_string()),
                    ],
                ));
            } else {
                self.stage.set_text(stage);
            }
            self.error.set_visible(s.last_error.is_some());
            self.error.set_text(match s.last_error.as_deref() {
                None => tr("Tidak ada error pada siklus terakhir."),
                Some("auth_required") => {
                    tr("Login Google perlu diperbarui. Gunakan login ulang untuk melanjutkan.")
                }
                Some("offline") => {
                    tr("Drive belum dapat dijangkau. Perubahan lokal tetap dicatat.")
                }
                Some(_) => tr(
                    "Ada operasi yang perlu diperiksa. Lihat status file dan diagnostik service.",
                ),
            });
        } else {
            if let Some(config) = snapshot.configuration() {
                self.profile.set_text(
                    &snapshot
                        .account
                        .as_ref()
                        .map(|a| fmt("Google Drive · {}", &[("0", (a.email).to_string())]))
                        .unwrap_or_else(|| {
                            fmt(
                                "Google Drive · Profil {}",
                                &[("0", (config.account).to_string())],
                            )
                        }),
                );
                self.folder.set_text(&config.root.to_string_lossy());
                self.remote.set_text(&fmt(
                    "Interval sinkronisasi: {} detik",
                    &[("0", (config.interval_seconds).to_string())],
                ));
            } else {
                self.profile.set_text(tr("Google Drive"));
                self.folder.set_text(tr("Belum dipilih"));
                self.remote.set_text(tr("Belum dipilih"));
            }
            self.pending.set_text("—");
            self.operations.set_text("—");
            self.success.set_text(tr("Belum ada"));
            self.stage.set_text(tr("Menunggu konfigurasi"));
            self.error.set_text(tr("Service belum tersedia."));
            self.error.set_visible(false);
        }
    }
}
fn launch_uri(uri: &str, feedback: &gtk::Label) {
    if gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>).is_err() {
        feedback.set_text(tr(
            "Folder atau browser tidak dapat dibuka. Periksa aplikasi default desktop.",
        ));
    }
}
#[derive(Clone)]
struct BuildRequest {
    state: PathBuf,
    snapshot_path: Option<PathBuf>,
    dialogs: (bool, bool, bool, bool, bool),
    background: bool,
    start_service: bool,
    language: String,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct OpenProfile {
    state: PathBuf,
    dialogs: (bool, bool, bool, bool, bool),
}
#[derive(Default)]
struct PageLifecycle {
    timer: Option<glib::SourceId>,
    close: Option<glib::SignalHandlerId>,
}
fn build(app: &gtk::Application, request: BuildRequest, reuse: Option<gtk::ApplicationWindow>) {
    if reuse.is_none() {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
    }
    let BuildRequest {
        state,
        snapshot_path,
        dialogs,
        background,
        start_service,
        language,
    } = request;
    let (show_setup, show_offline, show_progress, show_maintenance, show_accounts) = dialogs;
    let snapshot_mode = snapshot_path.is_some();
    let tray_state = state.canonicalize().unwrap_or_else(|_| state.clone());
    let setup_state = state.clone();
    let binary = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mygdrive-sync")))
    {
        Some(p) => p,
        None => return,
    };
    let relaunch = restart::Launcher::capture(&tray_state, &binary).ok();
    let controller = match dashboard::spawn_observer(state, binary.clone(), !snapshot_mode) {
        Ok(c) => Rc::new(c),
        Err(_) => return,
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    let window = reuse.unwrap_or_else(|| {
        gtk::ApplicationWindow::builder()
            .application(app)
            .title("MyGdrive Rust")
            .default_width(1080)
            .default_height(790)
            .build()
    });
    let lifecycle = Rc::new(RefCell::new(PageLifecycle::default()));
    let shell = column(0);
    let navigation = column(12);
    navigation.add_css_class("topbar");
    let brand = flow();
    brand.set_halign(gtk::Align::Fill);
    brand.set_max_children_per_line(3);
    brand.insert(&label("☁  MyGdrive Rust", "brand"), -1);
    let autostart = gtk::CheckButton::with_label(tr("Mulai saat login"));
    autostart.set_sensitive(false);
    brand.insert(&autostart, -1);
    let exit = gtk::Button::with_label(tr("Keluar"));
    exit.set_tooltip_text(Some(tr(
        "Selesaikan sinkronisasi aktif, hentikan layanan, lalu keluar.",
    )));
    brand.insert(&exit, -1);
    navigation.append(&brand);
    let nav = flow();
    nav.set_halign(gtk::Align::Fill);
    nav.insert(&label(tr("Dashboard"), "nav-item"), -1);
    let activity_button = gtk::Button::with_label(tr("Aktivitas sinkronisasi"));
    nav.insert(&activity_button, -1);
    let settings = gtk::Button::with_label(tr("Akun dan folder"));
    nav.insert(&settings, -1);
    let offline_button = gtk::Button::with_label(tr("Tersedia offline"));
    nav.insert(&offline_button, -1);
    let maintenance_button = gtk::Button::with_label(tr("Penyimpanan dan pemulihan"));
    nav.insert(&maintenance_button, -1);
    navigation.append(&nav);
    shell.append(&navigation);
    let content = column(12);
    content.add_css_class("content");
    content.set_hexpand(true);
    let heading = column(5);
    heading.append(&label(
        &fmt(
            "RUANG KERJA ANDA  ·  v{}",
            &[("0", (env!("CARGO_PKG_VERSION")).to_string())],
        ),
        "eyebrow",
    ));
    heading.append(&label(tr("Dashboard"), "title"));
    heading.append(&label(
        tr("Pantau file, kelola sinkronisasi, dan periksa pembaruan aplikasi di sini."),
        "subtitle",
    ));
    content.append(&heading);
    let status_card = card();
    status_card.add_css_class("hero");
    let status_header = row();
    let title = label(tr("Menghubungkan ke service…"), "status-title");
    title.set_hexpand(true);
    let pill = label(tr("Memeriksa"), "pill");
    status_header.append(&title);
    status_header.append(&pill);
    status_card.append(&status_header);
    let description = label(tr("Mengambil status sinkronisasi."), "subtitle");
    description.set_wrap(true);
    status_card.append(&description);
    let stage = label(tr("Menunggu status"), "detail");
    stage.set_wrap(true);
    status_card.append(&stage);
    let buttons = flow();
    let pause = gtk::Button::with_label(tr("Jeda"));
    let sync = gtk::Button::with_label(tr("Sinkronkan sekarang"));
    sync.add_css_class("suggested-action");
    let start = gtk::Button::with_label(tr("Mulai service"));
    let stop = gtk::Button::with_label(tr("Hentikan service"));
    for b in [&pause, &sync, &start, &stop] {
        b.set_sensitive(false);
        buttons.insert(b, -1);
    }
    status_card.append(&buttons);
    content.append(&status_card);
    let restart_request = Rc::new(RefCell::new(RestartRequest::default()));
    let updating = Rc::new(Cell::new(false));
    let request_update = restart_request.clone();
    content.append(&updater::panel(
        snapshot_mode,
        updating.clone(),
        move || {
            request_update.borrow_mut().update = true;
        },
    ));
    let request_language = restart_request.clone();
    content.append(&settings::panel(
        &language,
        snapshot_mode,
        move |language| {
            request_language.borrow_mut().language = Some(language);
        },
    ));
    let folder_card = card();
    folder_card.append(&label(tr("FOLDER TERHUBUNG"), "eyebrow"));
    let profile = label(tr("Google Drive"), "detail");
    folder_card.append(&profile);
    let folder = label(tr("Memuat…"), "detail");
    folder.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    folder.set_selectable(true);
    folder_card.append(&folder);
    let remote = label("", "subtitle");
    remote.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    folder_card.append(&remote);
    let links = flow();
    let open = gtk::Button::with_label(tr("Buka folder lokal"));
    let drive = gtk::Button::with_label(tr("Buka di Drive"));
    links.insert(&open, -1);
    links.insert(&drive, -1);
    folder_card.append(&links);
    content.append(&folder_card);
    let metrics = flow();
    metrics.set_halign(gtk::Align::Fill);
    metrics.set_homogeneous(true);
    metrics.set_max_children_per_line(3);
    let pending = label("—", "value");
    let operations = label("—", "value");
    let success = label(tr("Belum ada"), "detail");
    for (caption, value) in [
        (tr("FILE MENUNGGU"), &pending),
        (tr("OPERASI TERTUNDA"), &operations),
        (tr("TERAKHIR BERHASIL"), &success),
    ] {
        let metric = card();
        metric.set_hexpand(true);
        metric.append(&label(caption, "eyebrow"));
        metric.append(value);
        metrics.insert(&metric, -1);
    }
    content.append(&metrics);
    let activity_card = card();
    activity_card.append(&label(tr("AKTIVITAS SESI INI"), "eyebrow"));
    let error = label(tr("Menunggu hasil service."), "detail");
    error.set_wrap(true);
    status_card.append(&error);
    error.set_visible(false);
    let activity = label(tr("Belum ada aktivitas pada jendela ini."), "activity");
    activity.set_wrap(true);
    activity_card.append(&activity);
    let expander = gtk::Expander::new(Some(tr("Aktivitas sesi ini")));
    expander.set_child(Some(&activity_card));
    content.append(&expander);
    let notice=label(tr("File diunduh saat dibuka. Gunakan Tersedia offline agar file bisa dibuka tanpa internet. Perubahan lokal tetap disimpan saat koneksi terputus."),"notice");
    notice.set_wrap(true);
    content.append(&notice);
    let feedback = label(
        tr("Menutup jendela ini tetap membiarkan service berjalan."),
        "subtitle",
    );
    feedback.set_wrap(true);
    content.append(&feedback);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&content)
        .hexpand(true)
        .vexpand(true)
        .build();
    shell.append(&scroll);
    window.set_child(Some(&shell));
    {
        let weak = window.downgrade();
        let state = setup_state.clone();
        maintenance_button.connect_clicked(move |_| {
            if let Some(parent) = weak.upgrade() {
                maintenance::show(&parent, state.clone(), snapshot_mode);
            }
        });
    }
    {
        let weak = window.downgrade();
        let state = setup_state.clone();
        offline_button.connect_clicked(move |_| {
            if let Some(parent) = weak.upgrade() {
                offline::show(&parent, state.clone(), snapshot_mode);
            }
        });
    }
    let widgets = Rc::new(Widgets {
        title,
        pill,
        description,
        profile,
        folder,
        remote,
        pending,
        operations,
        success,
        stage,
        error,
        pause,
        sync,
        start,
        stop,
        open,
        drive,
    });
    let current = Rc::new(RefCell::new(Snapshot {
        live: false,
        status: None,
        config: None,
        account: None,
    }));
    let accounts_action = gio::SimpleAction::new("accounts", None);
    let parent = window.downgrade();
    let account_state = setup_state.clone();
    accounts_action.connect_activate(move |_, _| {
        if let Some(parent) = parent.upgrade() {
            setup::show(&parent, account_state.clone(), snapshot_mode);
        }
    });
    app.add_action(&accounts_action);
    let progress = Rc::new(progress::Popup::new(&window, snapshot_mode));
    let progress_action = gio::SimpleAction::new_stateful("progress", None, &false.to_variant());
    let popup_button = progress.clone();
    activity_button.connect_clicked(move |_| popup_button.present(None));
    let popup = progress.clone();
    progress_action.connect_activate(move |_, _| popup.present(None));
    let action_state = progress_action.clone();
    progress.window.connect_visible_notify(move |window| {
        action_state.set_state(&window.is_visible().to_variant())
    });
    let hide_progress = gio::SimpleAction::new("hide-progress", None);
    let popup = progress.clone();
    hide_progress.connect_activate(move |_, _| popup.window.set_visible(false));
    app.add_action(&hide_progress);
    app.add_action(&progress_action);
    let busy = Rc::new(Cell::new(false));
    let tray_ready = Rc::new(Cell::new(false));
    let quit_requested = Rc::new(Cell::new(false));
    let updating_autostart = Rc::new(Cell::new(false));
    let autostart_readable = Rc::new(Cell::new(false));
    let tray = if snapshot_mode {
        None
    } else {
        tray::spawn(
            &tray_state,
            tray::Model::from_snapshot(&current.borrow(), false),
        )
        .ok()
        .map(Rc::new)
    };
    {
        let quit_requested = quit_requested.clone();
        exit.connect_clicked(move |_| quit_requested.set(true));
    }
    // Real application actions also support desktop activation and keyboard navigation.
    for (name, hide) in [("show", false), ("hide", true)] {
        let action = gio::SimpleAction::new(name, None);
        let weak = window.downgrade();
        action.connect_activate(move |_, _| {
            if let Some(window) = weak.upgrade() {
                if hide {
                    window.close();
                } else {
                    window.present();
                }
            }
        });
        app.add_action(&action);
    }
    let quit_action = gio::SimpleAction::new("quit", None);
    let quit_flag = quit_requested.clone();
    quit_action.connect_activate(move |_, _| quit_flag.set(true));
    app.add_action(&quit_action);
    app.set_accels_for_action("app.hide", &["<Primary>w"]);
    app.set_accels_for_action("app.quit", &["<Primary>q"]);
    {
        let controller = controller.clone();
        let busy = busy.clone();
        let updating = updating_autostart.clone();
        let feedback = feedback.clone();
        autostart.connect_toggled(move |button| {
            if updating.get() {
                return;
            }
            if !busy.get()
                && controller
                    .commands
                    .try_send(Command::SetAutostart(button.is_active()))
                    .is_ok()
            {
                busy.set(true);
                button.set_sensitive(false);
                feedback.set_text(tr("Memperbarui mulai saat login…"));
            }
        });
    }
    for (button, command) in [
        (&widgets.pause, None),
        (&widgets.sync, Some(Command::SyncNow)),
        (&widgets.start, Some(Command::Start)),
        (&widgets.stop, Some(Command::Shutdown)),
    ] {
        let controller = controller.clone();
        let current = current.clone();
        let busy = busy.clone();
        let weak_widgets = Rc::downgrade(&widgets);
        let feedback = feedback.clone();
        button.connect_clicked(move |_| {
            let Some(widgets) = weak_widgets.upgrade() else {
                return;
            };
            if busy.get() {
                return;
            }
            let command = command.unwrap_or_else(|| {
                if current.borrow().status.as_ref().is_some_and(|s| s.paused) {
                    Command::Resume
                } else {
                    Command::Pause
                }
            });
            if controller.commands.try_send(command).is_ok() {
                busy.set(true);
                feedback.set_text(tr("Memproses permintaan…"));
                widgets.render(&current.borrow(), true);
            } else {
                feedback.set_text(tr(
                    "Antrean kontrol sedang penuh. Coba lagi setelah status diperbarui.",
                ));
            }
        });
    }
    {
        let current = current.clone();
        let feedback = feedback.clone();
        let folder_state = setup_state.clone();
        widgets.open.connect_clicked(move |_| {
            if let Some(config) = current.borrow().configuration() {
                let folder = mygdrive_sync::cloud_view::mounted_online_root(&folder_state)
                    .unwrap_or_else(|| config.root.clone());
                if let Ok(url) = reqwest::Url::from_file_path(folder) {
                    launch_uri(url.as_str(), &feedback);
                }
            }
        });
    }
    {
        let current = current.clone();
        let feedback = feedback.clone();
        widgets.drive.connect_clicked(move |_| {
            if let Some(config) = current.borrow().configuration() {
                if config.drive_folder == "root" {
                    launch_uri("https://drive.google.com/drive/my-drive", &feedback);
                    return;
                }
                let mut url = reqwest::Url::parse("https://drive.google.com/drive/folders/")
                    .expect("constant URL");
                url.path_segments_mut()
                    .expect("hierarchical URL")
                    .pop_if_empty()
                    .push(&config.drive_folder);
                launch_uri(url.as_str(), &feedback);
            }
        });
    }
    let switch_app = app.downgrade();
    let switch_window = window.downgrade();
    let switch_controller = controller.clone();
    let switch_tray = tray.clone();
    let switch_progress = progress.window.downgrade();
    let switch_lifecycle = lifecycle.clone();
    let switch_busy = busy.clone();
    let switch_updating = updating.clone();
    let switch_feedback = feedback.clone();
    let switch_language = language.clone();
    let pending_view = Rc::new(RefCell::new(None::<OpenProfile>));
    let selected_view = pending_view.clone();
    let selected: accounts::Select = Rc::new(move |state| {
        if switch_busy.get() || switch_updating.get() {
            switch_feedback.set_text(tr("Tunggu perintah akun selesai sebelum berpindah."));
            return false;
        }
        let (Some(app), Some(window)) = (switch_app.upgrade(), switch_window.upgrade()) else {
            return false;
        };
        switch_controller.stop();
        if let Some(tray) = &switch_tray {
            tray.stop();
        }
        if let Some(progress) = switch_progress.upgrade() {
            progress.destroy();
        }
        let mut lifecycle = switch_lifecycle.borrow_mut();
        if let Some(timer) = lifecycle.timer.take() {
            timer.remove();
        }
        if let Some(handler) = lifecycle.close.take() {
            window.disconnect(handler);
        }
        drop(lifecycle);
        for child in app.windows() {
            if child != window.clone().upcast::<gtk::Window>() {
                child.destroy();
            }
        }
        for name in [
            "accounts",
            "select-account",
            "open-profile",
            "progress",
            "hide-progress",
        ] {
            app.remove_action(name);
        }
        build(
            &app,
            BuildRequest {
                dialogs: selected_view
                    .borrow_mut()
                    .take()
                    .filter(|view| view.state == state)
                    .map(|view| view.dialogs)
                    .unwrap_or_default(),
                state,
                snapshot_path: None,
                background: false,
                start_service: false,
                language: switch_language.clone(),
            },
            Some(window),
        );
        true
    });
    let account_switcher = accounts::Switcher::new(
        &window,
        tray_state.clone(),
        binary.clone(),
        snapshot_mode,
        selected,
    )
    .ok()
    .map(Rc::new);
    if let Some(switcher) = &account_switcher {
        let header = row();
        header.set_hexpand(true);
        brand.set_hexpand(true);
        navigation.remove(&brand);
        header.append(&brand);
        header.append(&switcher.button);
        navigation.prepend(&header);
        let action = gio::SimpleAction::new("select-account", Some(glib::VariantTy::STRING));
        let switcher = switcher.clone();
        action.connect_activate(move |_, parameter| {
            if let Some(state) = parameter.and_then(|v| v.get::<String>()) {
                switcher.select(PathBuf::from(state));
            }
        });
        app.add_action(&action);
        let action = gio::SimpleAction::new("open-profile", Some(glib::VariantTy::STRING));
        let switcher = account_switcher.as_ref().unwrap().clone();
        action.connect_activate(move |_, parameter| {
            if let Some(view) = parameter
                .and_then(|v| v.get::<String>())
                .and_then(|text| serde_json::from_str::<OpenProfile>(&text).ok())
            {
                let state = view.state.clone();
                *pending_view.borrow_mut() = Some(view);
                if !switcher.select(state) {
                    pending_view.borrow_mut().take();
                }
            }
        });
        app.add_action(&action);
        let action = gio::SimpleAction::new("accounts", None);
        let switcher = account_switcher.as_ref().unwrap().clone();
        action.connect_activate(move |_, _| switcher.manage());
        app.add_action(&action);
        let switcher = account_switcher.as_ref().unwrap().clone();
        settings.connect_clicked(move |_| switcher.manage());
    }
    let restart_in_progress = Rc::new(Cell::new(false));
    let restart_for_close = restart_in_progress.clone();
    let mut restart_plan = None::<restart::Plan>;
    let mut desired_language = language;
    let controller_for_close = controller.clone();
    let tray_for_close = tray.clone();
    let tray_ready_for_close = tray_ready.clone();
    let app_for_events = app.clone();
    let weak = window.downgrade();
    let mut history = VecDeque::<String>::new();
    let progress_render = progress.window.clone();
    let progress_for_open = progress.clone();
    let mut last_cycle = None;
    let mut queued_files = 0;
    let mut background_pending =
        background && !show_setup && !show_offline && !show_progress && !snapshot_mode;
    let background_deadline = std::time::Instant::now() + Duration::from_secs(11);
    if start_service && !snapshot_mode && controller.commands.try_send(Command::Start).is_ok() {
        busy.set(true);
        feedback.set_text(tr("Memulai service dari konfigurasi tersimpan…"));
    }
    let page_accounts = account_switcher.clone();
    let page_timer = glib::timeout_add_local(Duration::from_millis(100), move || {
        if let Some(accounts) = &page_accounts {
            accounts
                .button
                .set_sensitive(!busy.get() && !updating.get());
        }
        let Some(window) = weak.upgrade() else {
            if let Some(tray) = &tray {
                tray.stop();
            }
            return glib::ControlFlow::Break;
        };
        if let Some(tray) = &tray {
            let available = tray.available();
            if tray_ready.replace(available) && !available {
                window.present();
                feedback.set_text(tr(
                    "Tray terputus. Dashboard ditampilkan agar aplikasi tetap dapat diakses.",
                ));
            }
            if background_pending && available {
                window.set_visible(false);
                background_pending = false;
            }
            tray.update(
                tray::Model::from_snapshot(&current.borrow(), busy.get())
                    .with_pending_files(queued_files),
            );
            for _ in 0..16 {
                match tray.events.try_recv() {
                    Ok(tray::Event::Show) => {
                        background_pending = false;
                        window.present();
                    }
                    Ok(tray::Event::Progress) => progress.present(None),
                    Ok(tray::Event::ProgressAt { x, y }) => progress.present(Some((x, y))),
                    Ok(tray::Event::OpenFolder) => widgets.open.emit_clicked(),
                    Ok(tray::Event::Quit) => quit_requested.set(true),
                    Ok(tray::Event::Control(command)) => {
                        if !busy.get() && controller.commands.try_send(command).is_ok() {
                            busy.set(true);
                            feedback.set_text(tr("Memproses permintaan tray…"));
                            widgets.render(&current.borrow(), true);
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        if background_pending && std::time::Instant::now() >= background_deadline {
            background_pending = false;
            window.present();
            feedback.set_text(tr("Tray belum tersedia. Dashboard tetap terbuka."));
        }
        if quit_requested.get() {
            *restart_request.borrow_mut() = RestartRequest::default();
            restart_plan = None;
        }
        if !busy.get() && !updating.get() {
            let mut request = restart_request.borrow_mut();
            if request.update || request.language.is_some() {
                if let Some(selected) = request.language.take() {
                    desired_language = selected;
                }
                let update = std::mem::take(&mut request.update);
                let prepared = relaunch.as_ref().ok_or(()).and_then(|launcher| {
                    launcher
                        .plan(
                            &desired_language,
                            !window.is_visible(),
                            progress.window.is_visible(),
                        )
                        .map_err(|_| ())
                });
                match prepared {
                    Ok(plan) if update => {
                        if controller
                            .commands
                            .try_send(Command::RestartCollectionAfterUpdate)
                            .is_ok()
                        {
                            restart_plan = Some(plan);
                            restart_in_progress.set(true);
                            busy.set(true);
                            feedback.set_text(tr("Update installed. Finishing synchronization before restarting automatically…"));
                            widgets.render(&current.borrow(), true);
                        } else {
                            feedback.set_text(tr("Automatic restart could not be completed. Choose Restart now to retry."));
                        }
                    }
                    Ok(plan) => {
                        if plan.spawn_after_exit().is_ok() {
                            controller.stop();
                            if let Some(tray) = &tray {
                                tray.stop();
                            }
                            app_for_events.quit();
                            return glib::ControlFlow::Break;
                        }
                        feedback.set_text(tr("Language saved. Reopen the application to use it."));
                    }
                    Err(()) => feedback.set_text(if update {
                        tr("Automatic restart could not be completed. Choose Restart now to retry.")
                    } else {
                        tr("Language saved. Reopen the application to use it.")
                    }),
                }
            }
        }
        if quit_requested.get()
            && !busy.get()
            && controller
                .commands
                .try_send(Command::ShutdownCollectionAndWait)
                .is_ok()
        {
            quit_requested.set(false);
            busy.set(true);
            window.present();
            feedback.set_text(tr(
                "Menunggu sinkronisasi aktif selesai dan service berhenti sebelum keluar…",
            ));
            widgets.render(&current.borrow(), true);
        }
        exit.set_sensitive(!busy.get());
        autostart.set_sensitive(
            !snapshot_mode
                && autostart_readable.get()
                && current.borrow().configuration().is_some()
                && !busy.get(),
        );
        for _ in 0..16 {
            match controller.events.try_recv() {
                Ok(Event::Snapshot(snapshot)) => {
                    if let Some(s) = &snapshot.status {
                        if snapshot.live
                            && s.last_sync_finished_ms.is_some()
                            && last_cycle != s.last_sync_finished_ms
                        {
                            last_cycle = s.last_sync_finished_ms;
                            let message = if let Some(r) = &s.last_report {
                                fmt(
                                    "{} · {} selesai, {} konflik, {} ditunda",
                                    &[
                                        ("0", (time_text(s.last_sync_finished_ms)).to_string()),
                                        ("1", (r.completed).to_string()),
                                        ("2", (r.conflicts).to_string()),
                                        ("3", (r.deferred).to_string()),
                                    ],
                                )
                            } else {
                                fmt(
                                    "{} · Siklus perlu diperiksa",
                                    &[("0", (time_text(s.last_sync_finished_ms)).to_string())],
                                )
                            };
                            history.push_front(message);
                            history.truncate(5);
                            activity
                                .set_text(&history.iter().cloned().collect::<Vec<_>>().join("\n"));
                        }
                    }
                    *current.borrow_mut() = *snapshot;
                    progress.refresh_status(&current.borrow());
                    widgets.render(&current.borrow(), busy.get());
                }
                Ok(Event::Activity(items)) => {
                    queued_files = items.pending_files;
                    progress.update(&items, &current.borrow());
                }
                Ok(Event::Completed(result)) => {
                    if restart_plan.take().is_some() {
                        restart_in_progress.set(false);
                        feedback.set_text(tr("Automatic restart could not be completed. Choose Restart now to retry."));
                        busy.set(false);
                        widgets.render(&current.borrow(), false);
                        continue;
                    }
                    busy.set(false);
                    feedback.set_text(match &result {
                        Ok(message) => message,
                        Err(message) => message,
                    });
                    widgets.render(&current.borrow(), false);
                }
                Ok(Event::Autostart(result)) => {
                    updating_autostart.set(true);
                    match result {
                        Ok(enabled) => {
                            autostart_readable.set(true);
                            autostart.set_active(enabled);
                        }
                        Err(message) => {
                            autostart_readable.set(false);
                            autostart.set_tooltip_text(Some(&message));
                        }
                    }
                    updating_autostart.set(false);
                }
                Ok(Event::RestartReady { start_service }) => {
                    if let Some(mut plan) = restart_plan.take() {
                        plan.start_service = false;
                        plan.restore_accounts = true;
                        if plan.spawn_after_exit().is_err() {
                            restart_in_progress.set(false);
                            busy.set(false);
                            feedback.set_text(tr("Automatic restart could not be completed. Choose Restart now to retry."));
                            // Restore the service if preparing the relaunch helper failed after drain.
                            if start_service && controller.commands.try_send(Command::Start).is_ok()
                            {
                                busy.set(true);
                            }
                            continue;
                        }
                    }
                    controller.stop();
                    if let Some(tray) = &tray {
                        tray.stop();
                    }
                    app_for_events.quit();
                    return glib::ControlFlow::Break;
                }
                Ok(Event::ExitReady) => {
                    controller.stop();
                    if let Some(tray) = &tray {
                        tray.stop();
                    }
                    app_for_events.quit();
                    return glib::ControlFlow::Break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    current.borrow_mut().live = false;
                    progress.refresh_status(&current.borrow());
                    if let Some(tray) = &tray {
                        tray.update(
                            tray::Model::from_snapshot(&current.borrow(), false)
                                .with_pending_files(queued_files),
                        );
                    }
                    busy.set(false);
                    widgets.render(&current.borrow(), false);
                    feedback.set_text(tr("Worker dashboard berhenti. Buka kembali aplikasi untuk menghubungkan ulang."));
                    return glib::ControlFlow::Break;
                }
            }
        }
        glib::ControlFlow::Continue
    });
    lifecycle.borrow_mut().timer = Some(page_timer);
    if !background
        || snapshot_mode
        || show_setup
        || show_offline
        || show_maintenance
        || show_accounts
    {
        window.present();
    }
    let render_window: gtk::Window = if show_accounts {
        account_switcher
            .as_ref()
            .and_then(|switcher| switcher.manager_window())
            .unwrap_or_else(|| window.clone().upcast())
    } else if show_maintenance {
        maintenance::show(&window, setup_state, snapshot_mode)
    } else if show_progress {
        progress_for_open.present(None);
        progress_render
    } else if show_offline {
        offline::show(&window, setup_state, snapshot_mode)
    } else if show_setup {
        setup::show(&window, setup_state, snapshot_mode)
    } else {
        window.clone().upcast()
    };
    if let Some(path) = snapshot_path {
        let weak = render_window.downgrade();
        let app = app.clone();
        glib::timeout_add_local_once(Duration::from_secs(4), move || {
            if let Some(window) = weak.upgrade() {
                let paintable = gtk::WidgetPaintable::new(Some(&window));
                let snapshot = gtk::Snapshot::new();
                paintable.snapshot(&snapshot, window.width() as f64, window.height() as f64);
                if let (Some(node), Some(renderer)) = (snapshot.to_node(), window.renderer()) {
                    match renderer.render_texture(&node, None).save_to_png(&path) {
                        Ok(()) => println!("Dashboard snapshot: {}", path.display()),
                        Err(e) => eprintln!("Snapshot failed: {e}"),
                    }
                } else {
                    eprintln!("Dashboard render node unavailable");
                }
            }
            app.quit();
        });
    }
    let close_controller = controller_for_close;
    let close_handler = window.connect_close_request(move |window| {
        if restart_for_close.get() {
            return glib::Propagation::Stop;
        }
        if tray_ready_for_close.get() {
            window.set_visible(false);
            return glib::Propagation::Stop;
        }
        close_controller.stop();
        if let Some(tray) = &tray_for_close {
            tray.stop();
        }
        glib::Propagation::Proceed
    });
    lifecycle.borrow_mut().close = Some(close_handler);
}
fn default_state() -> PathBuf {
    directories::BaseDirs::new()
        .expect("user data directory")
        .data_local_dir()
        .join("mygdrive")
}

fn absolute_state(state: PathBuf) -> PathBuf {
    state
        .canonicalize()
        .unwrap_or_else(|_| std::path::absolute(&state).unwrap_or(state))
}
fn request_for(args: &Args, state: PathBuf, language: &str) -> BuildRequest {
    BuildRequest {
        state,
        snapshot_path: args.snapshot.clone(),
        dialogs: (
            args.setup,
            args.offline,
            args.progress,
            args.maintenance,
            args.accounts,
        ),
        background: args.background,
        start_service: args.start_service && !args.restore_accounts,
        language: language.to_owned(),
    }
}
fn main() -> glib::ExitCode {
    let args = Args::parse();
    let language = args
        .language
        .clone()
        .or_else(|| {
            mygdrive_sync::preferences::directory()
                .and_then(|path| mygdrive_sync::preferences::language(&path).ok().flatten())
        })
        .unwrap_or_else(|| "auto".into());
    mygdrive_sync::i18n::initialize(Some(&language));
    let state = absolute_state(args.state.clone().unwrap_or_else(default_state));
    let collection = absolute_state(mygdrive_sync::account_manager::base_for(&state));
    let id = format!(
        "io.mygdrive.Accounts{}",
        mygdrive_sync::autostart::profile_id(&collection)
    );
    let flags = gio::ApplicationFlags::HANDLES_COMMAND_LINE
        | if args.snapshot.is_some() {
            gio::ApplicationFlags::NON_UNIQUE
        } else {
            gio::ApplicationFlags::empty()
        };
    let app = gtk::Application::builder()
        .application_id(&id)
        .flags(flags)
        .build();
    app.connect_startup(|_| theme::install());
    let initial = request_for(&args, state, &language);
    let activate = initial.clone();
    app.connect_activate(move |app| build(app, activate.clone(), None));
    app.connect_command_line(move |app, command_line| {
        let incoming = match Args::try_parse_from(command_line.arguments()) {
            Ok(args) => args,
            Err(_) => return glib::ExitCode::FAILURE,
        };
        let state = incoming.state.clone().unwrap_or_else(default_state);
        let state = absolute_state(if state.is_absolute() {
            state
        } else {
            command_line
                .cwd()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
                .join(state)
        });
        if absolute_state(mygdrive_sync::account_manager::base_for(&state)) != collection {
            return glib::ExitCode::FAILURE;
        }
        if incoming.restore_accounts {
            let base = collection.clone();
            let binary = std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|path| path.join("mygdrive-sync")));
            if let Some(binary) = binary {
                std::thread::spawn(move || {
                    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        let _ = runtime.block_on(
                            mygdrive_sync::account_manager::restore_collection(&base, &binary),
                        );
                    }
                });
            }
        }
        if app.windows().is_empty() {
            build(app, request_for(&incoming, state, &language), None);
        } else {
            if incoming.start_service && !incoming.restore_accounts {
                let base = collection.clone();
                let state = state.clone();
                let binary = std::env::current_exe()
                    .ok()
                    .and_then(|path| path.parent().map(|path| path.join("mygdrive-sync")));
                if let Some(binary) = binary {
                    std::thread::spawn(move || {
                        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        {
                            let _ = runtime.block_on(mygdrive_sync::account_manager::control(
                                &base,
                                &state,
                                &binary,
                                Command::Start,
                            ));
                        }
                    });
                }
            }
            if !incoming.background {
                let view = OpenProfile {
                    state,
                    dialogs: (
                        incoming.setup,
                        incoming.offline,
                        incoming.progress,
                        incoming.maintenance,
                        incoming.accounts,
                    ),
                };
                if let Ok(view) = serde_json::to_string(&view) {
                    app.activate_action("open-profile", Some(&view.to_variant()));
                }
                if let Some(window) = app
                    .windows()
                    .into_iter()
                    .find(|window| window.is::<gtk::ApplicationWindow>())
                {
                    window.present();
                }
            }
        }
        glib::ExitCode::SUCCESS
    });
    app.run_with_args(&std::env::args().collect::<Vec<_>>())
}

#[cfg(test)]
mod account_ui_tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, time::Instant};

    fn pump_until(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let context = glib::MainContext::default();
        while !predicate() {
            while context.pending() {
                context.iteration(false);
            }
            assert!(Instant::now() < deadline, "account UI did not converge");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn has_label(widget: &gtk::Widget, text: &str) -> bool {
        if widget
            .downcast_ref::<gtk::Label>()
            .is_some_and(|label| label.text() == text)
        {
            return true;
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            if has_label(&widget, text) {
                return true;
            }
            child = widget.next_sibling();
        }
        false
    }
    #[test]
    #[ignore = "requires a dedicated GTK display; use capture-previews.py --ui-test-binary"]
    fn switching_accounts_reuses_window_and_replaces_profile_observers() {
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join("config"));
        std::env::set_var("XDG_DATA_HOME", temp.path().join("data"));
        gtk::init().unwrap();
        let base = temp.path().join("collection");
        let mut states = Vec::new();
        for name in ["personal", "work"] {
            let state = base.join("profiles").join(name);
            let root = temp.path().join(name);
            fs::create_dir_all(&state).unwrap();
            fs::create_dir_all(&root).unwrap();
            fs::write(
                mygdrive_sync::service::config_path(&state),
                serde_json::to_vec(&mygdrive_sync::service::ServiceConfig {
                    version: 1,
                    account: "default".into(),
                    root,
                    drive_folder: "root".into(),
                    interval_seconds: 30,
                    cache_max_bytes: None,
                })
                .unwrap(),
            )
            .unwrap();
            let token = mygdrive_sync::oauth::token_path(&state, "default");
            fs::create_dir_all(token.parent().unwrap()).unwrap();
            fs::write(&token,serde_json::to_vec(&serde_json::json!({"client":{"client_id":"demo.apps.googleusercontent.com","client_secret":"synthetic"},"access_token":"synthetic","refresh_token":"synthetic","expires_at_ms":i64::MAX,"scope":mygdrive_sync::oauth::DRIVE_SCOPE,"drive_user_id":name,"identity":{"email":format!("{name}@example.test"),"display_name":name}})).unwrap()).unwrap();
            fs::set_permissions(token, fs::Permissions::from_mode(0o600)).unwrap();
            states.push(state);
        }
        let app = gtk::Application::builder()
            .application_id("io.mygdrive.AccountUiTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gio::Cancellable>).unwrap();
        build(
            &app,
            BuildRequest {
                state: states[0].clone(),
                snapshot_path: None,
                dialogs: (false, false, false, false, false),
                background: false,
                start_service: false,
                language: "en".into(),
            },
            None,
        );
        let window = app
            .windows()
            .into_iter()
            .find_map(|window| window.downcast::<gtk::ApplicationWindow>().ok())
            .unwrap();
        pump_until(|| has_label(window.upcast_ref(), "personal@example.test"));
        for index in [1, 0, 1] {
            let old = window.child().unwrap();
            app.activate_action(
                "select-account",
                Some(&states[index].to_string_lossy().to_string().to_variant()),
            );
            pump_until(|| window.child().is_some_and(|child| child != old));
            pump_until(|| {
                has_label(
                    window.upcast_ref(),
                    if index == 0 {
                        "personal@example.test"
                    } else {
                        "work@example.test"
                    },
                )
            });
            assert_eq!(
                app.windows()
                    .iter()
                    .filter(|window| window.is::<gtk::ApplicationWindow>())
                    .count(),
                1
            );
            assert!(app.windows().contains(&window.clone().upcast()));
        }
        let old = window.child().unwrap();
        let view = serde_json::to_string(&OpenProfile {
            state: states[0].clone(),
            dialogs: (false, false, false, false, true),
        })
        .unwrap();
        app.activate_action("open-profile", Some(&view.to_variant()));
        pump_until(|| window.child().is_some_and(|child| child != old));
        pump_until(|| has_label(window.upcast_ref(), "personal@example.test"));
        pump_until(|| {
            app.windows()
                .iter()
                .any(|window| window.title().as_deref() == Some(tr("Kelola akun · MyGdrive Rust")))
        });
        for window in app.windows() {
            window.destroy();
        }
        app.quit();
    }
}
