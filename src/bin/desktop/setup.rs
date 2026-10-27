use super::{card, column, flow, label};
use gtk::{glib, prelude::*};
use mygdrive_sync::i18n::{fmt, tr};
use mygdrive_sync::onboarding::{self, Command, Event, FolderPage};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

fn open_profile(state: &std::path::Path) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Ok(launcher) = mygdrive_sync::autostart::desktop_launcher(&exe) else {
        return false;
    };
    match std::process::Command::new(launcher)
        .env("MYGDRIVE_LANGUAGE", mygdrive_sync::i18n::current_language())
        .arg("--state")
        .arg(state)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}
fn choose(
    parent: &gtk::Window,
    action: gtk::FileChooserAction,
    callback: impl Fn(PathBuf) + 'static,
) {
    let chooser = gtk::FileChooserNative::new(
        Some(if action == gtk::FileChooserAction::SelectFolder {
            tr("Pilih folder lokal kosong")
        } else {
            tr("Pilih JSON client OAuth Desktop")
        }),
        Some(parent),
        action,
        Some(tr("Pilih")),
        Some(tr("Batal")),
    );
    if action == gtk::FileChooserAction::Open {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("OAuth client JSON"));
        filter.add_pattern("*.json");
        chooser.add_filter(&filter);
    }
    // Native dialogs are not owned by GTK's toplevel list. Retain until response.
    let retained = Rc::new(RefCell::new(Some(chooser.clone())));
    chooser.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Accept {
            if let Some(path) = dialog.file().and_then(|f| f.path()) {
                callback(path);
            }
        }
        dialog.destroy();
        retained.borrow_mut().take();
    });
    chooser.show();
}
struct Ui {
    worker: onboarding::Controller,
    busy: Cell<bool>,
    logged: Cell<bool>,
    existing: Cell<bool>,
    saved_client: Cell<bool>,
    account: gtk::Label,
    target: gtk::Label,
    saved: Cell<bool>,
    root: RefCell<Option<PathBuf>>,
    path: RefCell<Vec<(String, String)>>,
    next: RefCell<Option<String>>,
    folders: gtk::ListBox,
    breadcrumb: gtk::Label,
    feedback: gtk::Label,
    local: gtk::Label,
    login: gtk::Button,
    reauth: gtk::Button,
    back: gtk::Button,
    next_button: gtk::Button,
    reload: gtk::Button,
    save: gtk::Button,
    choose: gtk::Button,
}
impl Ui {
    fn update(&self) {
        let idle = !self.busy.get() && !self.saved.get();
        self.login.set_sensitive(idle);
        self.reauth.set_sensitive(idle);
        self.choose
            .set_sensitive(idle && self.logged.get() && !self.existing.get());
        self.reload.set_sensitive(idle && self.logged.get());
        self.back
            .set_sensitive(idle && self.logged.get() && self.path.borrow().len() > 1);
        self.next_button
            .set_sensitive(idle && self.next.borrow().is_some());
        self.folders.set_sensitive(idle && self.logged.get());
        self.save.set_sensitive(
            idle && self.logged.get() && !self.existing.get() && self.root.borrow().is_some(),
        );
        self.breadcrumb.set_text(
            &self
                .path
                .borrow()
                .iter()
                .map(|(_, n)| n.as_str())
                .collect::<Vec<_>>()
                .join(" / "),
        );
    }
    fn send(&self, command: Command, message: &str) {
        if self.busy.get() {
            return;
        }
        if self.worker.commands.try_send(command).is_ok() {
            self.busy.set(true);
            self.feedback.set_text(message);
        } else {
            self.feedback
                .set_text(tr("Worker belum tersedia. Tutup dan buka lagi pengaturan."));
        }
        self.update();
    }
    fn browse(&self, page: Option<String>) {
        let parent = self.path.borrow().last().expect("root folder").0.clone();
        while let Some(child) = self.folders.first_child() {
            self.folders.remove(&child);
        }
        *self.next.borrow_mut() = None;
        self.send(
            Command::Browse { parent, page },
            tr("Membaca folder Google Drive…"),
        );
    }
    fn fill(self: &Rc<Self>, page: FolderPage) {
        while let Some(child) = self.folders.first_child() {
            self.folders.remove(&child);
        }
        if page.files.is_empty() {
            self.folders.append(&label(
                tr("Tidak ada subfolder pada halaman ini."),
                "subtitle",
            ));
        }
        for folder in page.files {
            let button = gtk::Button::with_label(&format!("📁  {}", folder.name));
            button.set_halign(gtk::Align::Fill);
            if let Some(text) = button.child().and_downcast::<gtk::Label>() {
                text.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                text.set_max_width_chars(45);
                text.set_xalign(0.0);
            }
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.path
                        .borrow_mut()
                        .push((folder.id.clone(), folder.name.clone()));
                    ui.browse(None);
                }
            });
            self.folders.append(&button);
        }
        *self.next.borrow_mut() = page.next;
    }
}
/// Returns the native setup window, also used by the application's render-only QA mode.
pub fn show(parent: &gtk::ApplicationWindow, current: PathBuf, snapshot_mode: bool) -> gtk::Window {
    show_managed(parent, current, snapshot_mode, None, false)
}

pub fn show_managed(
    parent: &gtk::ApplicationWindow,
    current: PathBuf,
    snapshot_mode: bool,
    select: Option<super::accounts::Select>,
    add_account: bool,
) -> gtk::Window {
    let base = if current
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "profiles")
    {
        current
            .parent()
            .and_then(|p| p.parent())
            .expect("profile base")
            .to_path_buf()
    } else {
        current.clone()
    };
    let window = gtk::Window::builder()
        .title(tr("Akun dan folder · MyGdrive Rust"))
        .transient_for(parent)
        .modal(true)
        .default_width(690)
        .default_height(690)
        .build();
    window.set_application(parent.application().as_ref());
    window.set_destroy_with_parent(true);
    let content = column(8);
    content.add_css_class("content");
    let heading = label(tr("Akun dan folder"), "title");
    heading.set_focusable(true);
    content.append(&heading);
    let intro = label(
        tr("Hubungkan akun Google dan tentukan folder yang ingin Anda sinkronkan."),
        "subtitle",
    );
    intro.set_wrap(true);
    content.append(&intro);
    let account_card = card();
    account_card.append(&label(tr("AKUN GOOGLE"), "eyebrow"));
    let profiles_box = column(6);
    let profiles_expander =
        gtk::Expander::new(Some(tr("Akun tersimpan — pilih untuk melihat folder")));
    profiles_expander.set_child(Some(&profiles_box));
    profiles_expander.set_expanded(true);
    account_card.append(&profiles_expander);
    let login_row = flow();
    let login = gtk::Button::with_label(tr("Hubungkan akun baru…"));
    login.add_css_class("suggested-action");
    let reauth = gtk::Button::with_label(tr("Login ulang akun terpilih"));
    login_row.insert(&login, -1);
    login_row.insert(&reauth, -1);
    account_card.append(&login_row);
    let account = label(tr("Memeriksa akun tersimpan…"), "detail");
    account.set_selectable(true);
    account.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    account_card.append(&account);
    let target = label(tr("Belum ada pasangan folder tersimpan."), "subtitle");
    target.set_wrap(true);
    account_card.append(&target);
    content.append(&account_card);
    let drive_card = card();
    let scope=label(tr("Pilih My Drive untuk menampilkan seluruh drive, atau subfolder untuk membatasi tampilan. Konten diunduh saat dibuka atau dipilih tersedia offline. Profil baru tetap terpisah dari profil aktif."),"subtitle");
    scope.set_wrap(true);
    drive_card.append(&label(tr("1 · FOLDER GOOGLE DRIVE"), "eyebrow"));
    drive_card.append(&scope);

    let breadcrumb = label(tr("My Drive"), "detail");
    breadcrumb.set_wrap(true);
    drive_card.append(&breadcrumb);
    let nav = flow();
    let back = gtk::Button::with_label(tr("Naik"));
    let reload = gtk::Button::with_label(tr("Muat folder"));
    let next_button = gtk::Button::with_label(tr("Halaman berikutnya"));
    for b in [&back, &reload, &next_button] {
        nav.insert(b, -1);
    }
    drive_card.append(&nav);
    let folders = gtk::ListBox::new();
    folders.set_selection_mode(gtk::SelectionMode::None);
    folders.add_css_class("boxed-list");
    folders.append(&label(
        tr("Hubungkan akun untuk melihat folder."),
        "subtitle",
    ));
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(90)
        .max_content_height(180)
        .vexpand(true)
        .child(&folders)
        .build();
    drive_card.append(&list_scroll);
    let hint=label(tr("Folder pada jalur di atas adalah pilihan sinkronisasi. Klik subfolder untuk masuk; My Drive berarti seluruh drive pribadi."),"subtitle");
    hint.set_wrap(true);
    drive_card.append(&hint);
    content.append(&drive_card);
    let local_card = card();
    local_card.append(&label(tr("2 · FOLDER LOKAL"), "eyebrow"));
    let local = label(tr("Belum dipilih"), "detail");
    local.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    local_card.append(&local);
    let choose_button = gtk::Button::with_label(tr("Pilih folder lokal kosong…"));
    choose_button.set_halign(gtk::Align::Start);
    local_card.append(&choose_button);
    content.append(&local_card);
    let feedback = label(
        tr("Profil baru tidak mengganti folder yang sedang tersinkron."),
        "subtitle",
    );
    feedback.set_wrap(true);
    content.append(&feedback);
    let save = gtk::Button::with_label(tr("Simpan dan buka dashboard profil"));
    save.add_css_class("suggested-action");
    content.append(&save);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&content)
        .build();
    window.set_child(Some(&scroll));
    let worker = match onboarding::spawn_observer(base, current, snapshot_mode) {
        Ok(w) => w,
        Err(_) => {
            feedback.set_text(tr("Pengaturan tidak dapat dimulai."));
            window.present();
            return window;
        }
    };
    let ui = Rc::new(Ui {
        worker,
        busy: Cell::new(false),
        logged: Cell::new(false),
        existing: Cell::new(false),
        saved_client: Cell::new(false),
        account,
        target,
        saved: Cell::new(false),
        root: RefCell::new(None),
        path: RefCell::new(vec![("root".into(), tr("My Drive").into())]),
        next: RefCell::new(None),
        folders,
        breadcrumb,
        feedback,
        local,
        login,
        reauth,
        back,
        next_button,
        reload,
        save,
        choose: choose_button,
    });
    {
        let weak = Rc::downgrade(&ui);
        let parent = window.downgrade();
        ui.login.connect_clicked(move |_|{
            let (Some(ui),Some(parent))=(weak.upgrade(),parent.upgrade())else{return;};
            if ui.saved_client.get(){ui.send(Command::AddAccount,tr("Selesaikan login akun tambahan di browser."));return;}
            let weak=Rc::downgrade(&ui);choose(&parent,gtk::FileChooserAction::Open,move|client|{if let Some(ui)=weak.upgrade(){ui.send(Command::Login(client),tr("Selesaikan login dan persetujuan Google di browser. Menutup pengaturan membatalkan penantian (maksimum 15 menit)."));}});
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        ui.reauth.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.send(
                    Command::Reauthenticate,
                    tr("Selesaikan login ulang di browser dengan akun profil yang sama."),
                );
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        let parent = window.downgrade();
        ui.choose.connect_clicked(move |_| {
            let (Some(ui), Some(parent)) = (weak.upgrade(), parent.upgrade()) else {
                return;
            };
            let weak = Rc::downgrade(&ui);
            choose(&parent, gtk::FileChooserAction::SelectFolder, move |path| {
                if let Some(ui) = weak.upgrade() {
                    ui.local.set_text(&path.to_string_lossy());
                    *ui.root.borrow_mut() = Some(path);
                    ui.update();
                }
            });
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        ui.reload.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.browse(None);
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        ui.next_button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let page = ui.next.borrow().clone();
                ui.browse(page);
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        ui.back.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                if ui.path.borrow().len() > 1 {
                    ui.path.borrow_mut().pop();
                    ui.browse(None);
                }
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        ui.save.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let Some(root) = ui.root.borrow().clone() else {
                    return;
                };
                let folder = ui.path.borrow().last().expect("root").0.clone();
                ui.send(
                    Command::Save { root, folder },
                    tr("Memverifikasi folder dan menyimpan profil baru…"),
                );
            }
        });
    }
    ui.update();
    let weak_window = window.downgrade();
    let for_close = ui.clone();
    window.connect_close_request(move |_| {
        for_close.worker.stop();
        glib::Propagation::Proceed
    });
    let for_destroy = ui.clone();
    window.connect_destroy(move |_| for_destroy.worker.stop());
    let mut add_pending = add_account && !snapshot_mode;
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if weak_window.upgrade().is_none() {
            ui.worker.stop();
            return glib::ControlFlow::Break;
        }
        for _ in 0..8 {
            match ui.worker.events.try_recv() {
                Ok(Event::SignedOut {
                    identity,
                    config,
                    saved_client,
                }) => {
                    ui.busy.set(false);
                    ui.logged.set(false);
                    ui.saved_client.set(saved_client);
                    ui.existing.set(config.is_some());
                    ui.account.set_text(
                        &identity
                            .map(|identity| identity.email)
                            .unwrap_or_else(|| tr("Akun terputus").into()),
                    );
                    if let Some(config) = config {
                        ui.local.set_text(&config.root.to_string_lossy());
                        *ui.root.borrow_mut() = Some(config.root);
                    }
                    ui.feedback.set_text(tr("Akun diputuskan dari perangkat ini. Gunakan Hubungkan kembali dengan akun Google yang sama."));
                    ui.update();
                    if add_pending {
                        add_pending = false;
                        ui.login.emit_clicked();
                    }
                }
                Ok(Event::Accounts(accounts)) => {
                    while let Some(child) = profiles_box.first_child() {
                        profiles_box.remove(&child);
                    }
                    if accounts.is_empty() {
                        profiles_box.append(&label(tr("Belum ada akun tersimpan."), "subtitle"));
                        ui.account.set_text(tr("Hubungkan akun untuk memulai."));
                    }
                    for a in accounts {
                        let email = a
                            .identity
                            .as_ref()
                            .map(|i| i.email.as_str())
                            .unwrap_or(tr("Akun tersimpan — email belum dimuat"));
                        let caption = match &a.config {
                            Some(c) => format!("{} · {}", email, c.root.display()),
                            None => fmt(
                                "{} · lanjutkan memilih folder",
                                &[("0", (email).to_string())],
                            ),
                        };
                        let button = gtk::Button::with_label(&caption);
                        button.set_tooltip_text(Some(&caption));
                        if let Some(text) = button.child().and_downcast::<gtk::Label>() {
                            text.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                            text.set_max_width_chars(45);
                            text.set_xalign(0.0);
                        }
                        let weak = Rc::downgrade(&ui);
                        button.connect_clicked(move |_| {
                            if let Some(ui) = weak.upgrade() {
                                ui.send(
                                    Command::UseAccount(a.state.clone()),
                                    tr("Memuat akun tersimpan…"),
                                );
                            }
                        });
                        profiles_box.append(&button);
                    }
                }
                Ok(Event::Profiles(_)) => {}
                Ok(Event::Connected {
                    identity,
                    config,
                    folder,
                }) => {
                    ui.busy.set(false);
                    ui.logged.set(true);
                    ui.saved_client.set(true);
                    ui.saved.set(false);
                    ui.existing.set(config.is_some());
                    ui.account.set_text(
                        &identity
                            .map(|i| {
                                fmt("✓ Akun Google Drive: {}", &[("0", (i.email).to_string())])
                            })
                            .unwrap_or_else(|| tr("Akun tersimpan; email belum tersedia.").into()),
                    );
                    *ui.path.borrow_mut() = vec![("root".into(), tr("My Drive").into())];
                    if let Some(c) = config {
                        let f = folder.unwrap_or(onboarding::Folder {
                            id: c.drive_folder.clone(),
                            name: c.drive_folder.clone(),
                        });
                        ui.target.set_text(&fmt(
                            "Target tersimpan: {}\nFolder lokal: {}",
                            &[
                                ("0", (f.name).to_string()),
                                ("1", (c.root.display()).to_string()),
                            ],
                        ));
                        if c.drive_folder != "root" {
                            ui.path.borrow_mut().push((f.id, f.name));
                        }
                        ui.local.set_text(&c.root.to_string_lossy());
                        *ui.root.borrow_mut() = Some(c.root);
                    } else {
                        ui.target.set_text(tr("Akun berhasil dicatat. Pilih target Drive dan folder lokal untuk menyelesaikan profil."));
                        ui.local.set_text(tr("Belum dipilih"));
                        *ui.root.borrow_mut() = None;
                    }
                    if add_pending {
                        add_pending = false;
                        ui.update();
                        ui.login.emit_clicked();
                    } else if snapshot_mode {
                        while let Some(child) = ui.folders.first_child() {
                            ui.folders.remove(&child);
                        }
                        ui.folders.append(&label(
                            tr("Pratinjau: daftar folder dimuat saat dialog interaktif dibuka."),
                            "subtitle",
                        ));
                        ui.update();
                    } else {
                        ui.browse(None);
                    }
                }
                Ok(Event::Folders(page)) => {
                    ui.busy.set(false);
                    ui.fill(page);
                    ui.feedback.set_text(if ui.existing.get() { tr("Akun sudah terhubung. Target tersimpan ditampilkan di atas; daftar ini dapat dijelajahi tanpa mengubah cakupan sinkronisasi.") } else { tr("Pilih folder pada jalur di atas, kemudian pilih folder lokal kosong.") });
                    ui.update();
                }
                Ok(Event::Saved(state)) => {
                    ui.busy.set(false);
                    ui.saved.set(true);
                    if let Some(select) = &select {
                        if let Some(window) = weak_window.upgrade() {
                            window.close();
                        }
                        select(state);
                        return glib::ControlFlow::Break;
                    }
                    ui.feedback.set_text(if open_profile(&state){tr("Profil tersimpan. Mulai service dari dashboard baru saat Anda siap.")}else{tr("Profil tersimpan. Buka kembali melalui daftar profil untuk memulai service.")});
                    ui.update();
                }
                Ok(Event::LoginRequired) => {
                    ui.busy.set(false);
                    ui.logged.set(false);
                    ui.feedback.set_text(tr("Login Google diperlukan kembali. Gunakan Login ulang akun terpilih; akun tersimpan tidak dihapus."));
                    ui.update();
                }
                Ok(Event::Error(message)) => {
                    ui.busy.set(false);
                    ui.feedback.set_text(&message);
                    ui.update();
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    ui.busy.set(true);
                    ui.update();
                    return glib::ControlFlow::Break;
                }
            }
        }
        glib::ControlFlow::Continue
    });
    window.present();
    heading.grab_focus();
    window
}
