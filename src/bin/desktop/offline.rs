use super::{column, flow, label, launch_uri, row, time_text};
use gtk::{glib, prelude::*};
use mygdrive_sync::i18n::{fmt, tr};
use mygdrive_sync::{
    offline::{Node, Page},
    offline_controller::{self, Command, Event, Job},
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

struct Ui {
    worker: offline_controller::Controller,
    busy: Cell<bool>,
    preview: bool,
    page: RefCell<Option<Page>>,
    title: gtk::Label,
    summary: gtk::Label,
    feedback: gtk::Label,
    list: gtk::Box,
    back: gtk::Button,
    next: gtk::Button,
    reload: gtk::Button,
    current_pin: gtk::Button,
    open: gtk::Button,
    live: Cell<bool>,
    offset: Cell<usize>,
    revision: Cell<u64>,
}
fn state_text(node: &Node, complete: bool) -> String {
    let c = &node.counts;
    let status = if !complete {
        tr("Inventaris belum lengkap")
    } else if c.unsupported > 0 {
        tr("Ada format/izin yang belum didukung")
    } else if c.review > 0 {
        tr("Perlu tinjauan")
    } else if c.total == 0 {
        tr("Folder kosong pada inventaris")
    } else if !node.effective && c.ready == 0 {
        tr("Hanya online (konten belum diunduh)")
    } else if !node.effective && c.ready < c.total {
        tr("Sebagian lokal, sisanya hanya online")
    } else if c.ready == c.total {
        tr("File lokal tersedia")
    } else {
        tr("Menunggu unduhan/pemeriksaan")
    };
    fmt(
        "{status} · {}/{} file lokal · {} perubahan tertunda",
        &[
            ("0", (c.ready).to_string()),
            ("1", (c.total).to_string()),
            ("2", (c.dirty).to_string()),
            ("status", status.to_string()),
        ],
    )
}
impl Ui {
    fn send(&self, command: Command) {
        if self.busy.get() {
            return;
        }
        if self
            .worker
            .commands
            .try_send(Job {
                command,
                revision: self.revision.get() + 1,
            })
            .is_ok()
        {
            self.revision.set(self.revision.get() + 1);
            self.busy.set(true);
            self.feedback.set_text(tr("Memproses inventaris/pilihan…"));
            self.list.set_sensitive(false);
            for b in [&self.back, &self.next, &self.reload, &self.current_pin] {
                b.set_sensitive(false);
            }
        } else {
            self.feedback.set_text(tr(
                "Worker belum tersedia. Tutup dan buka kembali jendela ini.",
            ));
        }
    }
    fn choose(&self, target: String, pinned: bool) {
        let Some(page) = self.page.borrow().clone() else {
            return;
        };
        self.send(Command::Set {
            target,
            pinned,
            parent: page.current.key,
            offset: self.offset.get(),
        });
    }
    fn render(self: &Rc<Self>, page: Page, live: bool) {
        self.busy.set(false);
        self.live.set(live);
        self.title.set_text(&page.current.name);
        self.summary.set_text(&fmt(
            "{}\nInventaris diperiksa: {}",
            &[
                (
                    "0",
                    (state_text(&page.current, page.inventory_complete)).to_string(),
                ),
                ("1", (time_text(page.last_verified_ms)).to_string()),
            ],
        ));
        self.current_pin.set_label(if page.current.selected {
            tr("Lepas pilihan folder ini")
        } else if page.current.inherited {
            tr("Diwariskan dari folder induk")
        } else {
            tr("Jadikan folder ini tersedia offline")
        });
        self.current_pin.set_sensitive(
            live && !self.preview && (!page.current.inherited || page.current.selected),
        );
        self.back.set_sensitive(page.parent.is_some());
        self.next.set_sensitive(page.next.is_some());
        self.reload.set_sensitive(true);
        self.open.set_sensitive(true);
        self.list.set_sensitive(true);
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for node in &page.children {
            let entry = column(5);
            entry.add_css_class("card");
            let header = row();
            if node.folder {
                let name = gtk::Button::with_label(&format!("📁  {}", node.name));
                name.set_tooltip_text(Some(&node.key));
                if let Some(text) = name.child().and_downcast::<gtk::Label>() {
                    text.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                    text.set_max_width_chars(45);
                }
                name.set_hexpand(true);
                name.set_halign(gtk::Align::Fill);
                let weak = Rc::downgrade(self);
                let key = node.key.clone();
                name.connect_clicked(move |_| {
                    if let Some(ui) = weak.upgrade() {
                        ui.offset.set(0);
                        ui.send(Command::List {
                            parent: key.clone(),
                            offset: 0,
                        });
                    }
                });
                header.append(&name);
            } else {
                let name = label(&node.name, "detail");
                name.set_hexpand(true);
                name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                name.set_tooltip_text(Some(&node.key));
                header.append(&name);
            }
            let pin = gtk::Button::with_label(if node.selected {
                tr("Lepas pilihan")
            } else if node.inherited {
                tr("Diwariskan")
            } else {
                tr("Tersedia offline")
            });
            pin.set_sensitive(live && !self.preview && (!node.inherited || node.selected));
            let weak = Rc::downgrade(self);
            let key = node.key.clone();
            let pinned = !node.selected;
            pin.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.choose(key.clone(), pinned);
                }
            });
            header.append(&pin);
            entry.append(&header);
            let status = label(&state_text(node, page.inventory_complete), "subtitle");
            status.set_wrap(true);
            entry.append(&status);
            if node.effective {
                entry.append(&label(
                    if node.inherited {
                        tr("Pilihan induk berlaku, termasuk anak baru.")
                    } else {
                        tr("Pilihan offline aktif.")
                    },
                    "detail",
                ));
            }
            self.list.append(&entry);
        }
        if page.children.is_empty() {
            self.list.append(&label(
                tr("Belum ada file/subfolder pada halaman ini."),
                "subtitle",
            ));
        }
        self.feedback.set_text(if !live {
            tr("Mode baca inventaris terakhir. Mulai service untuk mengubah pilihan; Muat ulang memeriksa file lokal lagi.")
        } else if page.service_paused {
            tr("Service sedang dijeda. Pilihan tetap tersimpan; tekan Lanjutkan pada dashboard agar file/folder pilihan diunduh. Induk tidak perlu dipilih offline.")
        } else {
            tr("File atau subfolder dapat dipilih sendiri meskipun induknya hanya online. Pilihan folder mencakup semua anak dan anak baru. Tunggu status file lokal tersedia sebelum memutus internet.")
        });
        *self.page.borrow_mut() = Some(page);
    }
}
pub fn show(parent: &gtk::ApplicationWindow, state: PathBuf, preview: bool) -> gtk::Window {
    let window = gtk::Window::builder()
        .title(tr("Tersedia offline · MyGdrive Rust"))
        .transient_for(parent)
        .modal(true)
        .default_width(820)
        .default_height(690)
        .build();
    window.set_application(parent.application().as_ref());
    window.set_destroy_with_parent(true);
    let content = column(10);
    content.add_css_class("content");
    let heading = label(tr("Tersedia offline"), "title");
    heading.set_focusable(true);
    content.append(&heading);
    let intro=label(tr("Pilih file atau folder untuk digunakan tanpa internet. Memilih folder juga mengunduh seluruh isinya."),"subtitle");
    intro.set_wrap(true);
    content.append(&intro);
    let title = label(tr("Memuat inventaris…"), "status-title");
    title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    content.append(&title);
    let summary = label(tr("Memeriksa file lokal…"), "detail");
    summary.set_wrap(true);
    content.append(&summary);
    let nav = flow();
    let back = gtk::Button::with_label(tr("Naik"));
    let reload = gtk::Button::with_label(tr("Muat ulang"));
    let next = gtk::Button::with_label(tr("Halaman berikutnya"));
    let open = gtk::Button::with_label(tr("Buka folder lokal"));
    for b in [&back, &reload, &next, &open] {
        b.set_sensitive(false);
        nav.insert(b, -1);
    }
    content.append(&nav);
    let current_pin = gtk::Button::with_label(tr("Jadikan folder ini tersedia offline"));
    current_pin.add_css_class("suggested-action");
    current_pin.set_sensitive(false);
    content.append(&current_pin);
    let list = column(8);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list)
        .vexpand(true)
        .min_content_height(160)
        .build();
    content.append(&scroll);
    let notice_text = if std::env::var_os("FLATPAK_ID").is_some() {
        tr("Pilih tersedia offline untuk mengunduh konten ke folder lokal. Drive virtual dan menu Thunar tersedia pada paket .deb. Melepas pin mengantrekan pembersihan salinan bersih; perubahan tertunda dan file terbuka tetap dilindungi. File di Drive tetap ada. Offline Google Docs/Sheets/Slides belum didukung.")
    } else {
        tr("Konten diunduh saat dibuka melalui drive online atau dipilih offline. Melepas pin mengantrekan pembersihan file bersih dan cache terkait; perubahan tertunda, file terbuka, dan pin yang masih berlaku tetap dilindungi. File di Drive tetap ada dan dapat diunduh kembali. Offline Google Docs/Sheets/Slides belum didukung.")
    };
    let notice = label(notice_text, "notice");
    notice.set_wrap(true);
    let help = gtk::Expander::new(Some(tr("Cara kerja pilihan offline")));
    help.set_child(Some(&notice));
    content.append(&help);
    let feedback = label(tr("Menunggu inventaris service…"), "subtitle");
    feedback.set_wrap(true);
    content.append(&feedback);
    let outer = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&content)
        .build();
    window.set_child(Some(&outer));
    let worker = match offline_controller::spawn(state) {
        Ok(w) => w,
        Err(_) => {
            feedback.set_text(tr("Worker offline tidak dapat dimulai."));
            window.present();
            return window;
        }
    };
    let ui = Rc::new(Ui {
        worker,
        busy: Cell::new(false),
        preview,
        page: RefCell::new(None),
        title,
        summary,
        feedback,
        list,
        back,
        next,
        reload,
        current_pin,
        open,
        live: Cell::new(false),
        offset: Cell::new(0),
        revision: Cell::new(0),
    });
    {
        let weak = Rc::downgrade(&ui);
        ui.back.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let parent = ui.page.borrow().as_ref().and_then(|p| p.parent.clone());
                if let Some(parent) = parent {
                    ui.offset.set(0);
                    ui.send(Command::List { parent, offset: 0 });
                }
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        ui.reload.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let parent = ui
                    .page
                    .borrow()
                    .as_ref()
                    .map(|p| p.current.key.clone())
                    .unwrap_or_else(|| "root".into());
                ui.offset.set(0);
                ui.send(Command::List { parent, offset: 0 });
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        ui.next.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let page = ui.page.borrow().clone();
                if let Some(p) = page {
                    if let Some(offset) = p.next {
                        ui.offset.set(offset);
                        ui.send(Command::List {
                            parent: p.current.key,
                            offset,
                        });
                    }
                }
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        ui.current_pin.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let page = ui.page.borrow().clone();
                if let Some(p) = page {
                    ui.choose(p.current.key, !p.current.selected);
                }
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        ui.open.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                if let Some(p) = ui.page.borrow().as_ref() {
                    if let Ok(url) = reqwest::Url::from_file_path(&p.local_root) {
                        launch_uri(url.as_str(), &ui.feedback);
                    }
                }
            }
        });
    }
    let close = ui.clone();
    window.connect_close_request(move |_| {
        close.worker.stop();
        glib::Propagation::Proceed
    });
    let destroy = ui.clone();
    window.connect_destroy(move |_| destroy.worker.stop());
    let weak = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if weak.upgrade().is_none() {
            ui.worker.stop();
            return glib::ControlFlow::Break;
        }
        for _ in 0..8 {
            match ui.worker.events.try_recv() {
                Ok(Event::Page {
                    page,
                    live,
                    revision,
                }) => {
                    if revision >= ui.revision.get() {
                        ui.render(*page, live);
                    }
                }
                Ok(Event::Error { message, revision }) => {
                    if revision < ui.revision.get() {
                        continue;
                    }
                    ui.busy.set(false);
                    ui.live.set(false);
                    ui.feedback.set_text(&message);
                    ui.list.set_sensitive(false);
                    ui.current_pin.set_sensitive(false);
                    ui.reload.set_sensitive(true);
                    ui.back.set_sensitive(
                        ui.page
                            .borrow()
                            .as_ref()
                            .is_some_and(|p| p.parent.is_some()),
                    );
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return glib::ControlFlow::Break
                }
            }
        }
        glib::ControlFlow::Continue
    });
    window.present();
    heading.grab_focus();
    window
}
