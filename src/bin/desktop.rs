use clap::Parser;
use gtk::{gio, glib, prelude::*};
use mygdrive_sync::dashboard::{self, Command, Event, Snapshot};
use mygdrive_sync::tray;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::PathBuf,
    rc::Rc,
    time::Duration,
};
#[path = "desktop/offline.rs"]
mod offline;
#[path = "desktop/progress.rs"]
mod progress;
#[path = "desktop/setup.rs"]
mod setup;
#[path = "desktop/updater.rs"]
mod updater;

#[derive(Parser)]
#[command(about = "MyGdrive Rust desktop dashboard")]
struct Args {
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
}
const CSS: &str = r#"
window { background: #f4f6fb; color: #18243b; font-family: Sans; }
.sidebar { background: #14223b; color: #c4d2e9; padding: 28px 20px; }
.brand { font-size: 24px; font-weight: 800; color: white; }
.brand-caption { font-size: 12px; color: #94aacb; }
.nav-item { background: #28436d; color: white; border-radius: 10px; padding: 13px 16px; font-weight: 700; }
.sidebar-note { color: #a8bbd8; font-size: 12px; }
.sidebar checkbutton label { color: #dce7f7; }
.content { padding: 20px; }
.eyebrow { color: #657591; font-size: 11px; font-weight: 700; letter-spacing: 1px; }
.title { font-size: 27px; font-weight: 800; }
.subtitle { color: #65718a; font-size: 13px; }
.card { background: white; border: 1px solid #e1e6ef; border-radius: 15px; padding: 16px; }
.status-title { font-size: 22px; font-weight: 750; }
.value { font-size: 24px; font-weight: 750; }
.detail { font-size: 13px; }
.pill { background: #e6f6ef; color: #157a51; border-radius: 18px; padding: 7px 13px; font-size: 12px; font-weight: 700; }
.pill.warning { background: #fff0d9; color: #925b0d; }
.notice { background: #fff6e6; color: #755724; border-radius: 10px; padding: 10px; font-size: 12px; }
.activity { font-size: 12px; color: #53617b; }
button { border-radius: 9px; padding: 8px 12px; font-weight: 600; box-shadow: none; }
button.suggested-action { background: #346be8; color: white; border: 1px solid #346be8; }
button:disabled { opacity: 0.48; }
"#;
fn label(text: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class(class);
    label
}
fn row() -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, 12)
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
        .unwrap_or_else(|| "Belum ada".into())
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
        self.pill.set_text(if snapshot.live {
            if paused {
                "Dijeda"
            } else {
                "Service aktif"
            }
        } else {
            "Tidak terhubung"
        });
        if !snapshot.live || paused {
            self.pill.add_css_class("warning");
        } else {
            self.pill.remove_css_class("warning");
        }
        self.description.set_text(if snapshot.live {if paused{"Perubahan lokal tetap dicatat. Siklus yang aktif diselesaikan sebelum jeda."}else{"Folder Anda dipantau. Sinkronisasi mengikuti jadwal service."}}else if status.is_some(){"Menampilkan catatan terakhir. Mulai service untuk memperbarui status."}else if snapshot.configuration().is_some(){"Konfigurasi tersedia. Mulai service untuk memantau sinkronisasi."}else{"Belum ada konfigurasi sinkronisasi. Buka Akun dan folder untuk menghubungkan Google Drive."});
        self.pause
            .set_label(if paused { "Lanjutkan" } else { "Jeda" });
        self.pause.set_sensitive(active && !busy);
        self.sync.set_sensitive(active && !paused && !busy);
        self.stop.set_sensitive(active && !busy);
        self.start
            .set_sensitive(!snapshot.live && snapshot.configuration().is_some() && !busy);
        self.open.set_sensitive(snapshot.configuration().is_some());
        self.drive.set_sensitive(snapshot.configuration().is_some());
        if let Some(s) = status {
            self.profile.set_text(
                &snapshot
                    .account
                    .as_ref()
                    .map(|a| format!("Google Drive · {}", a.email))
                    .unwrap_or_else(|| format!("Google Drive · Profil {}", s.config.account)),
            );
            self.folder.set_text(&s.config.root.to_string_lossy());
            self.remote.set_text(&format!(
                "Interval sinkronisasi: {} detik",
                s.config.interval_seconds
            ));
            self.pending.set_text(&s.pending_files.to_string());
            self.operations.set_text(&s.pending_operations.to_string());
            self.success.set_text(&time_text(s.last_success_ms));
            let stage = if !snapshot.live {
                "Catatan terakhir"
            } else if s.paused {
                "Watcher lokal aktif"
            } else if !s.cycle_active {
                "Menunggu siklus berikutnya"
            } else {
                match s.progress.stage.as_str() {
                    "local_scan" => "Memindai perubahan lokal",
                    "inventory" => "Membaca daftar file Drive",
                    "change_feed" => "Memeriksa perubahan Drive",
                    "reconciling" => "Menyelaraskan file",
                    _ => "Menunggu siklus berikutnya",
                }
            };
            if s.progress.inventory_pages > 0 {
                self.stage.set_text(&format!(
                    "{stage} · {} halaman / {} item inventaris",
                    s.progress.inventory_pages, s.progress.inventory_items
                ));
            } else {
                self.stage.set_text(stage);
            }
            self.error.set_visible(s.last_error.is_some());
            self.error.set_text(match s.last_error.as_deref() {
                None => "Tidak ada error pada siklus terakhir.",
                Some("auth_required") => {
                    "Login Google perlu diperbarui. Gunakan login ulang untuk melanjutkan."
                }
                Some("offline") => "Drive belum dapat dijangkau. Perubahan lokal tetap dicatat.",
                Some(_) => {
                    "Ada operasi yang perlu diperiksa. Lihat status file dan diagnostik service."
                }
            });
        } else {
            if let Some(config) = snapshot.configuration() {
                self.profile.set_text(
                    &snapshot
                        .account
                        .as_ref()
                        .map(|a| format!("Google Drive · {}", a.email))
                        .unwrap_or_else(|| format!("Google Drive · Profil {}", config.account)),
                );
                self.folder.set_text(&config.root.to_string_lossy());
                self.remote.set_text(&format!(
                    "Interval sinkronisasi: {} detik",
                    config.interval_seconds
                ));
            } else {
                self.profile.set_text("Google Drive");
                self.folder.set_text("Belum dipilih");
                self.remote.set_text("Belum dipilih");
            }
            self.pending.set_text("—");
            self.operations.set_text("—");
            self.success.set_text("Belum ada");
            self.stage.set_text("Menunggu konfigurasi");
            self.error.set_text("Service belum tersedia.");
            self.error.set_visible(false);
        }
    }
}
fn launch_uri(uri: &str, feedback: &gtk::Label) {
    if gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>).is_err() {
        feedback
            .set_text("Folder atau browser tidak dapat dibuka. Periksa aplikasi default desktop.");
    }
}
fn build(
    app: &gtk::Application,
    state: PathBuf,
    snapshot_path: Option<PathBuf>,
    dialogs: (bool, bool, bool),
    background: bool,
    start_service: bool,
) {
    if let Some(window) = app
        .active_window()
        .or_else(|| app.windows().into_iter().next())
    {
        window.present();
        return;
    }
    let (show_setup, show_offline, show_progress) = dialogs;
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
    let controller = match dashboard::spawn_observer(state, binary, !snapshot_mode) {
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
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("MyGdrive Rust")
        .default_width(1080)
        .default_height(790)
        .build();
    let shell = row();
    shell.set_spacing(0);
    let sidebar = column(12);
    sidebar.add_css_class("sidebar");
    sidebar.set_width_request(180);
    sidebar.append(&label("☁  MyGdrive Rust", "brand"));
    sidebar.append(&label("YOUR CLOUD, YOUR DESKTOP", "brand-caption"));
    let space = gtk::Box::new(gtk::Orientation::Vertical, 0);
    space.set_height_request(30);
    sidebar.append(&space);
    sidebar.append(&label("Sinkronisasi", "nav-item"));
    let settings = gtk::Button::with_label("Akun dan folder");
    sidebar.append(&settings);
    let offline_button = gtk::Button::with_label("Tersedia offline");
    sidebar.append(&offline_button);
    let autostart = gtk::CheckButton::with_label("Mulai saat login");
    autostart.set_sensitive(false);
    sidebar.append(&autostart);
    if !snapshot_mode {
        if let Some(path) = updater::status_path() {
            let update_status = label("Pembaruan otomatis aktif", "sidebar-note");
            update_status.set_wrap(true);
            update_status.set_max_width_chars(24);
            if std::env::var_os("MYGDRIVE_DISABLE_AUTO_UPDATE").is_some() {
                update_status.set_text("Pembaruan otomatis dinonaktifkan");
            } else {
                let weak = update_status.downgrade();
                glib::timeout_add_local(Duration::from_secs(5), move || {
                    let Some(label) = weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        if let Ok(status) = serde_json::from_str::<serde_json::Value>(&text) {
                            if let Some(message) = status["message"].as_str() {
                                label.set_text(message);
                            }
                        }
                    }
                    glib::ControlFlow::Continue
                });
            }
            sidebar.append(&update_status);
        }
    }
    let exit = gtk::Button::with_label("Keluar");
    exit.set_tooltip_text(Some(
        "Hentikan sinkronisasi profil ini dengan tertib, kemudian keluar.",
    ));
    sidebar.append(&exit);
    let fill = gtk::Box::new(gtk::Orientation::Vertical, 0);
    fill.set_vexpand(true);
    sidebar.append(&fill);
    let note = label("Google Drive\nDashboard awal · Linux", "sidebar-note");
    note.set_wrap(true);
    sidebar.append(&note);
    shell.append(&sidebar);
    let content = column(12);
    content.add_css_class("content");
    content.set_hexpand(true);
    let heading = column(5);
    heading.append(&label("RUANG KERJA ANDA", "eyebrow"));
    heading.append(&label("Sinkronisasi Anda", "title"));
    heading.append(&label(
        "Default hanya online. Pilih file atau folder lewat Tersedia offline.",
        "subtitle",
    ));
    content.append(&heading);
    let status_card = card();
    let status_header = row();
    let title = label("Menghubungkan ke service…", "status-title");
    title.set_hexpand(true);
    let pill = label("Memeriksa", "pill");
    status_header.append(&title);
    status_header.append(&pill);
    status_card.append(&status_header);
    let description = label("Mengambil status sinkronisasi.", "subtitle");
    description.set_wrap(true);
    status_card.append(&description);
    let stage = label("Menunggu status", "detail");
    stage.set_wrap(true);
    status_card.append(&stage);
    let buttons = row();
    let pause = gtk::Button::with_label("Jeda");
    let sync = gtk::Button::with_label("Sinkronkan sekarang");
    sync.add_css_class("suggested-action");
    let start = gtk::Button::with_label("Mulai service");
    let stop = gtk::Button::with_label("Hentikan service");
    for b in [&pause, &sync, &start, &stop] {
        b.set_sensitive(false);
        buttons.append(b);
    }
    status_card.append(&buttons);
    content.append(&status_card);
    let folder_card = card();
    folder_card.append(&label("FOLDER TERHUBUNG", "eyebrow"));
    let profile = label("Google Drive", "detail");
    folder_card.append(&profile);
    let folder = label("Memuat…", "detail");
    folder.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    folder.set_selectable(true);
    folder_card.append(&folder);
    let remote = label("", "subtitle");
    remote.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    folder_card.append(&remote);
    let links = row();
    let open = gtk::Button::with_label("Buka folder lokal");
    let drive = gtk::Button::with_label("Buka di Drive");
    links.append(&open);
    links.append(&drive);
    folder_card.append(&links);
    content.append(&folder_card);
    let metrics = row();
    let pending = label("—", "value");
    let operations = label("—", "value");
    let success = label("Belum ada", "detail");
    for (caption, value) in [
        ("FILE PERLU PERHATIAN", &pending),
        ("OPERASI TERTUNDA", &operations),
        ("SINKRONISASI BERHASIL", &success),
    ] {
        let metric = card();
        metric.set_hexpand(true);
        metric.append(&label(caption, "eyebrow"));
        metric.append(value);
        metrics.append(&metric);
    }
    content.append(&metrics);
    let activity_card = card();
    activity_card.append(&label("AKTIVITAS SESI INI", "eyebrow"));
    let error = label("Menunggu hasil service.", "detail");
    error.set_wrap(true);
    status_card.append(&error);
    error.set_visible(false);
    let activity = label("Belum ada aktivitas pada jendela ini.", "activity");
    activity.set_wrap(true);
    activity_card.append(&activity);
    let expander = gtk::Expander::new(Some("Aktivitas sesi ini"));
    expander.set_child(Some(&activity_card));
    content.append(&expander);
    let notice=label("Jika koneksi terputus, perubahan lokal dan antrean operasi dipertahankan. Sinkronisasi dicoba ulang otomatis. Perubahan berbeda pada kedua sisi disimpan sebagai salinan konflik.","notice");
    notice.set_wrap(true);
    content.append(&notice);
    let feedback = label(
        "Menutup jendela ini tetap membiarkan service berjalan.",
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
        offline_button.connect_clicked(move |_| {
            if let Some(parent) = weak.upgrade() {
                offline::show(&parent, state.clone(), snapshot_mode);
            }
        });
    }
    {
        let weak = window.downgrade();
        let state = setup_state.clone();
        settings.connect_clicked(move |_| {
            if let Some(parent) = weak.upgrade() {
                setup::show(&parent, state.clone(), snapshot_mode);
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
    let popup = progress.clone();
    progress_action.connect_activate(move |_, _| popup.window.present());
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
                feedback.set_text("Memperbarui mulai saat login…");
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
                feedback.set_text("Memproses permintaan…");
                widgets.render(&current.borrow(), true);
            } else {
                feedback
                    .set_text("Antrean kontrol sedang penuh. Coba lagi setelah status diperbarui.");
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
    let controller_for_close = controller.clone();
    let tray_for_close = tray.clone();
    let tray_ready_for_close = tray_ready.clone();
    let app_for_events = app.clone();
    let weak = window.downgrade();
    let mut history = VecDeque::<String>::new();
    let progress_render = progress.window.clone();
    let mut last_cycle = None;
    let mut queued_files = 0;
    let mut background_pending =
        background && !show_setup && !show_offline && !show_progress && !snapshot_mode;
    let background_deadline = std::time::Instant::now() + Duration::from_secs(11);
    if start_service && !snapshot_mode && controller.commands.try_send(Command::Start).is_ok() {
        busy.set(true);
        feedback.set_text("Memulai service dari konfigurasi tersimpan…");
    }
    glib::timeout_add_local(Duration::from_millis(100), move || {
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
                feedback.set_text(
                    "Tray terputus. Dashboard ditampilkan agar aplikasi tetap dapat diakses.",
                );
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
                    Ok(tray::Event::Progress) => {
                        progress.window.present();
                    }
                    Ok(tray::Event::OpenFolder) => widgets.open.emit_clicked(),
                    Ok(tray::Event::Quit) => quit_requested.set(true),
                    Ok(tray::Event::Control(command)) => {
                        if !busy.get() && controller.commands.try_send(command).is_ok() {
                            busy.set(true);
                            feedback.set_text("Memproses permintaan tray…");
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
            feedback.set_text("Tray belum tersedia. Dashboard tetap terbuka.");
        }
        if quit_requested.get()
            && !busy.get()
            && controller
                .commands
                .try_send(Command::ShutdownAndWait)
                .is_ok()
        {
            quit_requested.set(false);
            busy.set(true);
            window.present();
            feedback.set_text(
                "Menunggu sinkronisasi aktif selesai dan service berhenti sebelum keluar…",
            );
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
                                format!(
                                    "{} · {} selesai, {} konflik, {} ditunda",
                                    time_text(s.last_sync_finished_ms),
                                    r.completed,
                                    r.conflicts,
                                    r.deferred
                                )
                            } else {
                                format!(
                                    "{} · Siklus perlu diperiksa",
                                    time_text(s.last_sync_finished_ms)
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
                    feedback.set_text("Worker dashboard berhenti. Buka kembali aplikasi untuk menghubungkan ulang.");
                    return glib::ControlFlow::Break;
                }
            }
        }
        glib::ControlFlow::Continue
    });
    if !background || snapshot_mode || show_setup || show_offline {
        window.present();
    }
    let render_window: gtk::Window = if show_progress {
        progress_render.present();
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
    window.connect_close_request(move |window| {
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
}
fn main() -> glib::ExitCode {
    let args = Args::parse();
    if args.snapshot.is_none() {
        updater::start();
    }
    let state = args.state.unwrap_or_else(|| {
        directories::BaseDirs::new()
            .expect("user data directory")
            .data_local_dir()
            .join("mygdrive")
    });
    let identity_state = state.canonicalize().unwrap_or_else(|_| {
        if state.is_absolute() {
            state.clone()
        } else {
            std::env::current_dir()
                .expect("current directory")
                .join(&state)
        }
    });
    let id = format!(
        "io.mygdrive.Profile{}",
        mygdrive_sync::autostart::profile_id(&identity_state)
    );
    let app = gtk::Application::builder()
        .application_id(&id)
        .flags(if args.snapshot.is_some() {
            gio::ApplicationFlags::NON_UNIQUE
        } else {
            gio::ApplicationFlags::empty()
        })
        .build();
    app.connect_activate(move |app| {
        build(
            app,
            state.clone(),
            args.snapshot.clone(),
            (args.setup, args.offline, args.progress),
            args.background,
            args.start_service,
        )
    });
    app.run_with_args(&["mygdrive-desktop"])
}
