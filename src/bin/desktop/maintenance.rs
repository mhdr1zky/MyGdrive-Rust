use super::{card, column, flow, label};
use gtk::{gio, glib, prelude::*};
use mygdrive_sync::{
    i18n::{fmt, tr},
    maintenance::{self, Outcome, Page, Request},
    service,
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

enum Command {
    Refresh { offset: usize, storage: bool },
    Apply(Request),
    OpenFolder(String),
    Retry,
}
enum Event {
    Page(Box<Page>),
    Done(Outcome),
    Open(String),
    Retried,
    Error,
}
struct Ui {
    commands: tokio::sync::mpsc::Sender<Command>,
    busy: Cell<bool>,
    preview: bool,
    page: RefCell<Option<Page>>,
    usage: gtk::Label,
    summary: gtk::Label,
    feedback: gtk::Label,
    list: gtk::Box,
    reload: gtk::Button,
    next: gtk::Button,
    first: gtk::Button,
    clean: gtk::Button,
    retry: gtk::Button,
}
fn bytes(value: u64) -> String {
    let mut value = value as f64;
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}
impl Ui {
    fn send(&self, command: Command) {
        if self.preview || self.busy.get() {
            return;
        }
        if self.commands.try_send(command).is_ok() {
            self.busy.set(true);
            self.sensitivity();
            self.feedback.set_text(tr("Memproses…"));
        }
    }
    fn sensitivity(&self) {
        let ready = !self.preview && !self.busy.get();
        self.reload.set_sensitive(ready);
        self.first.set_sensitive(ready);
        self.next.set_sensitive(
            ready
                && self
                    .page
                    .borrow()
                    .as_ref()
                    .is_some_and(|p| p.next.is_some()),
        );
        self.clean
            .set_sensitive(ready && self.page.borrow().as_ref().is_some_and(|p| p.live));
        self.retry.set_sensitive(
            ready
                && self
                    .page
                    .borrow()
                    .as_ref()
                    .is_some_and(|p| p.live && !p.paused),
        );
        self.list.set_sensitive(ready);
    }
    fn render(self: &Rc<Self>, mut page: Page, parent: &gtk::Window) {
        if page.storage.is_none() {
            page.storage = self.page.borrow().as_ref().and_then(|p| p.storage.clone());
        }
        if let Some(storage) = &page.storage {
            let text = [
                (tr("File lokal"), storage.backing.bytes),
                (tr("Cache"), storage.cache.bytes),
                (tr("Antrean unggahan"), storage.outbox.bytes),
                (tr("Salinan pemulihan"), storage.recovery.bytes),
                (tr("Data aplikasi lainnya"), storage.other.bytes),
            ]
            .into_iter()
            .map(|(title, value)| format!("{title}: {}", bytes(value)))
            .collect::<Vec<_>>()
            .join("  ·  ");
            let free = [
                (tr("Ruang kosong folder lokal"), storage.free_backing_bytes),
                (tr("Ruang kosong data aplikasi"), storage.free_state_bytes),
            ]
            .into_iter()
            .map(|(title, value)| {
                format!(
                    "{title}: {}",
                    value.map(bytes).unwrap_or_else(|| "—".into())
                )
            })
            .collect::<Vec<_>>()
            .join("  ·  ");
            let incomplete = if storage.skipped > 0 {
                tr("Sebagian path dilewati; ukuran belum lengkap.")
            } else {
                ""
            };
            self.usage
                .set_text(&format!("{text}\n{free}\n{incomplete}"));
        }
        self.summary.set_text(&fmt(
            "{} file perlu diperiksa.",
            &[("0", page.total_issues.to_string())],
        ));
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        if page.issues.is_empty() {
            self.list.append(&label(
                tr("Tidak ada masalah file pada halaman ini."),
                "notice",
            ));
        }
        for issue in &page.issues {
            let item = card();
            let path = label(&issue.file.relative_path, "detail");
            path.set_wrap(true);
            path.set_selectable(true);
            item.append(&path);
            let detail = label(
                if let Some(diagnostic) = issue.diagnostic {
                    diagnostic.description()
                } else {
                    tr("Periksa perubahan lokal dan versi Drive sebelum melanjutkan.")
                },
                "subtitle",
            );
            detail.set_wrap(true);
            item.append(&detail);
            let actions = flow();
            let open = gtk::Button::with_label(tr("Buka folder lokal"));
            let relative = issue.file.relative_path.clone();
            let weak = Rc::downgrade(self);
            open.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.send(Command::OpenFolder(relative.clone()));
                }
            });
            actions.insert(&open, -1);
            if let Some(uri) = issue
                .file
                .remote_id
                .as_deref()
                .and_then(maintenance::drive_uri)
            {
                let drive = gtk::Button::with_label(tr("Buka di Drive"));
                drive.connect_clicked(move |_| {
                    let _ =
                        gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
                });
                actions.insert(&drive, -1);
            }
            if page.live && issue.file.status == "recovery_review" && issue.file.review_required {
                if let Some(fp) = &issue.file.fingerprint {
                    let approve = gtk::Button::with_label(tr("Tinjau salinan pemulihan"));
                    approve.add_css_class("suggested-action");
                    let command = Request::ApproveRecovery {
                        file_id: issue.file.id.clone(),
                        generation: issue.file.generation,
                        sha256: fp.digest.sha256.clone(),
                    };
                    let weak = Rc::downgrade(self);
                    let parent = parent.downgrade();
                    approve.connect_clicked(move |_| {
                        let (Some(ui), Some(parent)) = (weak.upgrade(), parent.upgrade()) else { return; };
                        let dialog = gtk::MessageDialog::builder().transient_for(&parent).modal(true)
                            .message_type(gtk::MessageType::Question).text(tr("Izinkan salinan ini disinkronkan?"))
                            .secondary_text(tr("Periksa isi file melalui folder lokal terlebih dahulu. Persetujuan mengizinkan salinan pemulihan diunggah saat sinkronisasi dilanjutkan. Versi file yang berubah setelah daftar dimuat harus ditinjau ulang."))
                            .build();
                        dialog.add_button(tr("Batal"), gtk::ResponseType::Cancel);
                        dialog.add_button(tr("Setujui sinkronisasi"), gtk::ResponseType::Accept);
                        dialog.set_default_response(gtk::ResponseType::Cancel);
                        let weak = Rc::downgrade(&ui); let command = command.clone();
                        dialog.connect_response(move |dialog, response| {
                            if response == gtk::ResponseType::Accept { if let Some(ui) = weak.upgrade() { ui.send(Command::Apply(command.clone())); } }
                            dialog.close();
                        });
                        dialog.present();
                    });
                    actions.insert(&approve, -1);
                }
            }
            item.append(&actions);
            self.list.append(&item);
        }
        *self.page.borrow_mut() = Some(page);
        self.busy.set(false);
        self.sensitivity();
    }
}

pub fn show(parent: &gtk::ApplicationWindow, state: PathBuf, preview: bool) -> gtk::Window {
    let window = gtk::Window::builder()
        .title(tr("Penyimpanan dan pemulihan · MyGdrive Rust"))
        .transient_for(parent)
        .modal(true)
        .default_width(880)
        .default_height(700)
        .build();
    window.set_application(parent.application().as_ref());
    window.set_destroy_with_parent(true);
    let content = column(12);
    content.add_css_class("content");
    content.append(&label(tr("Penyimpanan dan pemulihan"), "title"));
    let usage = label(tr("Menghitung penggunaan disk…"), "detail");
    usage.set_wrap(true);
    content.append(&usage);
    let note = label(tr("Ukuran adalah perkiraan blok disk yang digunakan profil ini saat diperiksa. Pembersihan hanya menghapus cache bersih yang dapat diunduh kembali; file offline, perubahan tertunda, file terbuka dan salinan pemulihan tetap dilindungi."), "notice");
    note.set_wrap(true);
    content.append(&note);
    let buttons = flow();
    let reload = gtk::Button::with_label(tr("Hitung ulang dan muat ulang"));
    let clean = gtk::Button::with_label(tr("Bersihkan cache aman"));
    let retry = gtk::Button::with_label(tr("Sinkronkan sekarang"));
    let first = gtk::Button::with_label(tr("Halaman pertama"));
    let next = gtk::Button::with_label(tr("Halaman berikutnya"));
    for button in [&reload, &clean, &retry, &first, &next] {
        button.set_sensitive(false);
        buttons.insert(button, -1);
    }
    content.append(&buttons);
    let summary = label(tr("Memuat masalah file…"), "status-title");
    content.append(&summary);
    let list = column(8);
    content.append(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .vexpand(true)
            .min_content_height(160)
            .build(),
    );
    let feedback = label("", "subtitle");
    feedback.set_wrap(true);
    content.append(&feedback);
    window.set_child(Some(&content));
    let (commands, mut input) = tokio::sync::mpsc::channel(8);
    let (events, output) = mpsc::channel();
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let worker = std::thread::Builder::new().name("mygdrive-maintenance".into()).spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { let _ = events.send(Event::Error); return; };
        runtime.block_on(async move {
            let _ = events.send(match maintenance::browse(&state, 0, true).await { Ok(page) => Event::Page(Box::new(page)), Err(_) => Event::Error });
            loop {
                let command = tokio::select! { _=stop.cancelled()=>break, command=input.recv()=>match command { Some(command)=>command, None=>break } };
                let work = async {
                    match command {
                        Command::Refresh { offset, storage } => {
                            let _ = events.send(match maintenance::browse(&state, offset, storage).await { Ok(page) => Event::Page(Box::new(page)), Err(_) => Event::Error });
                        }
                        Command::Apply(command) => match maintenance::apply(&state, command).await {
                            Ok(outcome) => { let _ = events.send(Event::Done(outcome)); let _ = events.send(match maintenance::browse(&state, 0, true).await { Ok(page) => Event::Page(Box::new(page)), Err(_) => Event::Error }); }
                            Err(_) => { let _ = events.send(Event::Error); }
                        }
                        Command::OpenFolder(relative) => {
                            let result = service::load_config(&state).and_then(|config| maintenance::folder_uri(&config.root, &relative));
                            let _ = events.send(match result { Ok(uri)=>Event::Open(uri), Err(_)=>Event::Error });
                        }
                        Command::Retry => {
                            let result = async {
                                let status = service::send(&state, &service::Request::new(service::Action::Status)).await?;
                                let mut request = service::Request::new(service::Action::SyncNow);
                                request.session_id = Some(status.status.session_id);
                                let reply = service::send(&state, &request).await?;
                                if reply.accepted { Ok(()) } else { Err(mygdrive_sync::Error::Changed) }
                            }.await;
                            let _ = events.send(if result.is_ok() { Event::Retried } else { Event::Error });
                        }
                    }
                };
                tokio::select! { _=stop.cancelled()=>break, _=work=>{} }
            }
        });
    });
    if worker.is_err() {
        feedback.set_text(tr("Worker pemulihan tidak dapat dimulai."));
        window.present();
        return window;
    }
    let ui = Rc::new(Ui {
        commands,
        busy: Cell::new(true),
        preview,
        page: RefCell::new(None),
        usage,
        summary,
        feedback,
        list,
        reload,
        next,
        first,
        clean,
        retry,
    });
    for (button, include_storage) in [(&ui.reload, true), (&ui.first, false)] {
        let weak = Rc::downgrade(&ui);
        button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.send(Command::Refresh {
                    offset: 0,
                    storage: include_storage,
                });
            }
        });
    }
    let weak = Rc::downgrade(&ui);
    ui.next.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            let offset = ui.page.borrow().as_ref().and_then(|page| page.next);
            if let Some(offset) = offset {
                ui.send(Command::Refresh {
                    offset,
                    storage: false,
                });
            }
        }
    });
    let weak = Rc::downgrade(&ui);
    ui.clean.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.send(Command::Apply(Request::PruneCache { max_bytes: 0 }));
        }
    });
    let weak = Rc::downgrade(&ui);
    ui.retry.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.send(Command::Retry);
        }
    });
    let close = cancel.clone();
    window.connect_close_request(move |_| {
        close.cancel();
        glib::Propagation::Proceed
    });
    let destroy = cancel.clone();
    window.connect_destroy(move |_| destroy.cancel());
    let weak = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(window) = weak.upgrade() else {
            cancel.cancel();
            return glib::ControlFlow::Break;
        };
        for _ in 0..8 {
            match output.try_recv() {
                Ok(Event::Page(page)) => ui.render(*page, &window),
                Ok(Event::Done(outcome)) => ui.feedback.set_text(&match outcome {
                    Outcome::RecoveryApproved => {
                        tr("Salinan disetujui. Sinkronisasi mengikuti keadaan jeda layanan.").into()
                    }
                    Outcome::CachePruned {
                        reclaimed_bytes, ..
                    } => fmt(
                        "Cache dibersihkan: {}. Data yang dilindungi tetap disimpan.",
                        &[("0", bytes(reclaimed_bytes))],
                    ),
                }),
                Ok(Event::Open(uri)) => {
                    let _ =
                        gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
                    ui.busy.set(false);
                    ui.sensitivity();
                    ui.feedback.set_text("");
                }
                Ok(Event::Retried) => {
                    ui.busy.set(false);
                    ui.sensitivity();
                    ui.feedback.set_text(tr("Permintaan sinkronisasi dikirim."));
                }
                Ok(Event::Error) => {
                    ui.busy.set(false);
                    ui.sensitivity();
                    ui.feedback.set_text(tr("Belum terkonfirmasi. Muat ulang daftar; periksa apakah file berubah atau layanan perlu dimulai ulang."));
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return glib::ControlFlow::Break,
            }
        }
        glib::ControlFlow::Continue
    });
    window.present();
    window
}
