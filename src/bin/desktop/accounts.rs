use super::{card, column, label, row, setup};
use gtk::{glib, prelude::*};
use mygdrive_sync::{
    account_manager::{self, Account, Command, Event},
    dashboard,
    i18n::{fmt, tr},
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

pub type Select = Rc<dyn Fn(PathBuf) -> bool>;
pub struct Switcher {
    pub button: gtk::MenuButton,
    ui: Rc<Ui>,
}
struct Ui {
    worker: account_manager::Controller,
    current: PathBuf,
    parent: glib::WeakRef<gtk::ApplicationWindow>,
    preview: bool,
    select: Select,
    popover: gtk::Popover,
    list: gtk::Box,
    title: gtk::Label,
    avatar: gtk::Label,
    count: gtk::Label,
    feedback: gtk::Label,
    rows: RefCell<Vec<Account>>,
    manager: RefCell<Option<glib::WeakRef<gtk::Window>>>,
    manager_list: RefCell<Option<gtk::Box>>,
    busy: Cell<bool>,
    stopped: Cell<bool>,
    signature: RefCell<String>,
}
fn avatar(name: &str, email: &str) -> gtk::Label {
    let text: String = name
        .split_whitespace()
        .filter_map(|part| part.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    let avatar = label(if text.is_empty() { "G" } else { &text }, "account-avatar");
    let tint = email.bytes().fold(0usize, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(byte as usize)
    }) % 5;
    avatar.add_css_class(&format!("avatar-{tint}"));
    avatar.set_valign(gtk::Align::Center);
    avatar
}
impl Ui {
    fn send(&self, command: Command) -> bool {
        if self.preview || self.busy.get() {
            return false;
        }
        if self.worker.commands.try_send(command).is_ok() {
            self.busy.set(true);
            self.feedback.set_text(tr("Memproses permintaan akun…"));
            return true;
        }
        false
    }
    fn setup(&self, state: PathBuf, add: bool) {
        self.popover.popdown();
        if let Some(parent) = self.parent.upgrade() {
            setup::show_managed(&parent, state, self.preview, Some(self.select.clone()), add);
        }
    }
    fn render(self: &Rc<Self>, accounts: Vec<Account>) {
        self.busy.set(false);
        let signature = format!(
            "{:?}",
            accounts
                .iter()
                .map(|account| (
                    &account.state,
                    account.email(),
                    account.title(),
                    account.status(),
                    account.snapshot.status.as_ref().map(|s| (
                        s.pending_files,
                        s.paused,
                        s.shutdown_requested
                    ))
                ))
                .collect::<Vec<_>>()
        );
        *self.rows.borrow_mut() = accounts;
        if *self.signature.borrow() == signature {
            return;
        }
        *self.signature.borrow_mut() = signature;
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.count.set_text(&fmt(
            "{} akun terhubung",
            &[(
                "0",
                self.rows
                    .borrow()
                    .iter()
                    .filter(|account| !account.signed_out)
                    .count()
                    .to_string(),
            )],
        ));
        for account in self.rows.borrow().iter() {
            let active = account.state == self.current;
            if active {
                self.title.set_text(if account.email().is_empty() {
                    account.title()
                } else {
                    account.email()
                });
                self.avatar.set_text(
                    &account
                        .title()
                        .chars()
                        .next()
                        .unwrap_or('G')
                        .to_uppercase()
                        .to_string(),
                );
            }
            let button = gtk::Button::new();
            button.add_css_class("account-choice");
            if active {
                button.add_css_class("selected-account");
            }
            let content = row();
            content.append(&avatar(account.title(), account.email()));
            let text = column(2);
            text.set_hexpand(true);
            let name = label(account.title(), "detail");
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text.append(&name);
            let email = label(account.email(), "subtitle");
            email.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            text.append(&email);
            let status = label(account.status(), "subtitle");
            status.set_wrap(true);
            text.append(&status);
            content.append(&text);
            if active {
                content.append(&label("✓", "account-check"));
            }
            button.set_child(Some(&content));
            button.set_sensitive(!self.preview);
            let state = account.state.clone();
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.popover.popdown();
                    ui.send(Command::Select(state.clone()));
                }
            });
            self.list.append(&button);
        }
        if self.rows.borrow().is_empty() {
            self.list.append(&label(
                tr("Tambahkan akun Google untuk mulai menyinkronkan."),
                "subtitle",
            ));
        }
        self.render_manager();
    }
    fn render_manager(self: &Rc<Self>) {
        let Some(list) = self.manager_list.borrow().clone() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        for account in self.rows.borrow().iter() {
            let item = card();
            let heading = row();
            heading.append(&avatar(account.title(), account.email()));
            let identity = column(2);
            identity.set_hexpand(true);
            identity.append(&label(account.title(), "detail"));
            let email = label(account.email(), "subtitle");
            email.set_selectable(true);
            identity.append(&email);
            heading.append(&identity);
            let status = label(account.status(), "pill");
            if account.signed_out || !account.snapshot.live {
                status.add_css_class("warning");
            }
            heading.append(&status);
            item.append(&heading);
            if let Some(config) = account.snapshot.configuration() {
                let path = label(&config.root.to_string_lossy(), "subtitle");
                path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                path.set_tooltip_text(Some(&config.root.to_string_lossy()));
                item.append(&path);
            }
            let actions = super::flow();
            let open = gtk::Button::with_label(if account.configured {
                tr("Buka akun")
            } else {
                tr("Selesaikan pengaturan")
            });
            let state = account.state.clone();
            let configured = account.configured;
            let weak = Rc::downgrade(self);
            open.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    if configured {
                        ui.send(Command::Select(state.clone()));
                    } else {
                        ui.setup(state.clone(), false);
                    }
                }
            });
            actions.insert(&open, -1);
            if account.signed_out {
                let reconnect = gtk::Button::with_label(tr("Hubungkan kembali"));
                let state = account.state.clone();
                let weak = Rc::downgrade(self);
                reconnect.connect_clicked(move |_| {
                    if let Some(ui) = weak.upgrade() {
                        ui.setup(state.clone(), false);
                    }
                });
                actions.insert(&reconnect, -1);
            } else if account.configured {
                let paused = account.snapshot.status.as_ref().is_some_and(|s| s.paused);
                let (caption, command) = if !account.snapshot.live {
                    (tr("Mulai service"), dashboard::Command::Start)
                } else if paused {
                    (tr("Lanjutkan"), dashboard::Command::Resume)
                } else {
                    (tr("Jeda"), dashboard::Command::Pause)
                };
                let control = gtk::Button::with_label(caption);
                let state = account.state.clone();
                let weak = Rc::downgrade(self);
                control.connect_clicked(move |_| {
                    if let Some(ui) = weak.upgrade() {
                        ui.send(Command::Control {
                            state: state.clone(),
                            command,
                        });
                    }
                });
                actions.insert(&control, -1);
                let disconnect = gtk::Button::with_label(tr("Putuskan akun"));
                let state = account.state.clone();
                let email = account.email().to_owned();
                let weak = Rc::downgrade(self);
                disconnect.connect_clicked(move |_|{
                    let Some(ui)=weak.upgrade()else{return;};let Some(parent)=ui.parent.upgrade()else{return;};
                    let dialog=gtk::MessageDialog::builder().transient_for(&parent).modal(true).message_type(gtk::MessageType::Question)
                        .text(fmt("Putuskan akun {}?",&[("0",email.clone())]))
                        .secondary_text(tr("Sinkronisasi akun ini akan diselesaikan lalu dihentikan, token login lokal dihapus, dan mulai saat login dinonaktifkan. File lokal, file Drive, dan akun lain tetap tersimpan. Untuk menghubungkan kembali, gunakan akun Google yang sama."))
                        .build();dialog.add_button(tr("Batal"),gtk::ResponseType::Cancel);dialog.add_button(tr("Putuskan akun"),gtk::ResponseType::Accept);dialog.set_default_response(gtk::ResponseType::Cancel);
                    let weak=Rc::downgrade(&ui);let state=state.clone();dialog.connect_response(move |dialog,response|{if response==gtk::ResponseType::Accept{if let Some(ui)=weak.upgrade(){ui.send(Command::Disconnect(state.clone()));}}dialog.close();});dialog.present();
                });
                actions.insert(&disconnect, -1);
            }
            actions.set_sensitive(!self.preview && !self.busy.get());
            item.append(&actions);
            list.append(&item);
        }
    }
    fn manage(self: &Rc<Self>) -> Option<gtk::Window> {
        self.popover.popdown();
        if let Some(window) = self
            .manager
            .borrow()
            .as_ref()
            .and_then(|weak| weak.upgrade())
        {
            window.present();
            return Some(window);
        }
        let parent = self.parent.upgrade()?;
        let window = gtk::Window::builder()
            .title(tr("Kelola akun · MyGdrive Rust"))
            .transient_for(&parent)
            .modal(true)
            .default_width(780)
            .default_height(800)
            .build();
        window.set_application(parent.application().as_ref());
        window.set_destroy_with_parent(true);
        let content = column(12);
        content.add_css_class("content");
        content.append(&label(tr("Kelola akun"), "title"));
        let description=label(tr("Setiap akun memiliki folder dan sinkronisasi sendiri. Berpindah akun tidak menghentikan akun lain."),"subtitle");
        description.set_wrap(true);
        content.append(&description);
        let list = column(10);
        content.append(
            &gtk::ScrolledWindow::builder()
                .child(&list)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .build(),
        );
        let add = gtk::Button::with_label(tr("Tambahkan akun lain"));
        add.add_css_class("suggested-action");
        add.set_sensitive(!self.preview);
        let weak = Rc::downgrade(self);
        add.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.setup(ui.current.clone(), true);
            }
        });
        content.append(&add);
        content.append(&self.feedback);
        window.set_child(Some(&content));
        *self.manager.borrow_mut() = Some(window.downgrade());
        *self.manager_list.borrow_mut() = Some(list);
        self.render_manager();
        let weak = Rc::downgrade(self);
        window.connect_destroy(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.manager_list.borrow_mut().take();
                ui.manager.borrow_mut().take();
            }
        });
        window.present();
        Some(window)
    }
}
impl Switcher {
    pub fn new(
        parent: &gtk::ApplicationWindow,
        current: PathBuf,
        binary: PathBuf,
        preview: bool,
        select: Select,
    ) -> mygdrive_sync::Result<Self> {
        let worker = account_manager::spawn(account_manager::base_for(&current), binary, preview)?;
        let button = gtk::MenuButton::new();
        button.set_tooltip_text(Some(tr("Ganti akun Google")));
        button.add_css_class("account-menu");
        let heading = row();
        let avatar = avatar("G", "");
        let title = label(tr("Akun Google"), "detail");
        title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        title.set_max_width_chars(28);
        heading.append(&avatar);
        heading.append(&title);
        heading.append(&label("⌄", "subtitle"));
        button.set_child(Some(&heading));
        let popover = gtk::Popover::new();
        popover.set_position(gtk::PositionType::Bottom);
        button.set_popover(Some(&popover));
        let menu = column(10);
        menu.set_width_request(360);
        menu.add_css_class("account-popover");
        menu.append(&label("MyGdrive Rust", "eyebrow"));
        let count = label(tr("Memuat akun…"), "subtitle");
        menu.append(&count);
        let list = column(4);
        menu.append(
            &gtk::ScrolledWindow::builder()
                .child(&list)
                .max_content_height(430)
                .propagate_natural_height(true)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build(),
        );
        let add = gtk::Button::with_label(tr("Tambahkan akun lain"));
        let manage = gtk::Button::with_label(tr("Kelola akun"));
        menu.append(&add);
        menu.append(&manage);
        popover.set_child(Some(&menu));
        let feedback = label("", "subtitle");
        feedback.set_wrap(true);
        let ui = Rc::new(Ui {
            worker,
            current,
            parent: parent.downgrade(),
            preview,
            select,
            popover,
            list,
            title,
            avatar,
            count,
            feedback,
            rows: RefCell::new(Vec::new()),
            manager: RefCell::new(None),
            manager_list: RefCell::new(None),
            busy: Cell::new(false),
            stopped: Cell::new(false),
            signature: RefCell::new(String::new()),
        });
        let weak = Rc::downgrade(&ui);
        add.set_sensitive(!preview);
        add.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.setup(ui.current.clone(), true);
            }
        });
        let weak = Rc::downgrade(&ui);
        manage.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.manage();
            }
        });
        let weak = Rc::downgrade(&ui);
        let _timer = glib::timeout_add_local(Duration::from_millis(150), move || {
            let Some(ui) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if ui.stopped.get() {
                return glib::ControlFlow::Break;
            }
            for _ in 0..8 {
                match ui.worker.events.try_recv() {
                    Ok(Event::Accounts(accounts)) => ui.render(accounts),
                    Ok(Event::Selected(state)) => {
                        ui.busy.set(false);
                        if (ui.select)(state) {
                            return glib::ControlFlow::Break;
                        }
                        if let Some(parent) = ui.parent.upgrade() {
                            parent.present();
                        }
                    }
                    Ok(Event::Completed(result)) => {
                        ui.busy.set(false);
                        ui.feedback.set_text(&result.unwrap_or_else(|error| error));
                        ui.signature.borrow_mut().clear();
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return glib::ControlFlow::Break
                    }
                }
            }
            glib::ControlFlow::Continue
        });
        Ok(Self { button, ui })
    }
    pub fn select(&self, state: PathBuf) -> bool {
        self.ui.send(Command::Select(state))
    }
    pub fn manage(&self) {
        self.ui.manage();
    }
    pub fn manager_window(&self) -> Option<gtk::Window> {
        self.ui.manage()
    }
    pub fn stop(&self) {
        self.ui.stopped.set(true);
        self.ui.worker.stop();
    }
}
impl Drop for Switcher {
    fn drop(&mut self) {
        self.stop();
    }
}
