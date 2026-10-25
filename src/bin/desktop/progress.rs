use super::{card, column, flow, label, row};
use gtk::{gio, prelude::*};
use mygdrive_sync::dashboard::{ActivityItem, ActivityPage, Snapshot};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
};

pub struct Popup {
    pub window: gtk::Window,
    status: gtk::Label,
    summary: gtk::Label,
    bar: gtk::ProgressBar,
    list: gtk::Box,
    known: RefCell<HashSet<String>>,
    initialized: Cell<bool>,
    signature: RefCell<String>,
    last_notified: Cell<i64>,
    snapshot_mode: bool,
    pending_count: Cell<usize>,
    retry_count: Cell<usize>,
    delete_count: Cell<usize>,
}
impl Popup {
    pub fn new(parent: &gtk::ApplicationWindow, snapshot_mode: bool) -> Self {
        let window = gtk::Window::builder()
            .title("Aktivitas sinkronisasi · MyGdrive Rust")
            .default_width(560)
            .default_height(580)
            .resizable(true)
            .hide_on_close(true)
            .build();
        window.set_application(parent.application().as_ref());
        let content = column(12);
        content.add_css_class("content");
        content.append(&label("TRANSFER & RIWAYAT", "eyebrow"));
        content.append(&label("Aktivitas sinkronisasi", "title"));
        let health = card();
        health.add_css_class("hero");
        let status = label("Menunggu status layanan…", "detail");
        status.set_wrap(true);
        health.append(&status);
        let summary = label("Belum ada aktivitas.", "subtitle");
        summary.set_wrap(true);
        health.append(&summary);
        let bar = gtk::ProgressBar::new();
        health.append(&bar);
        content.append(&health);
        content.append(&label("AKTIVITAS FILE TERBARU", "eyebrow"));
        let list = column(8);
        let scroll = gtk::ScrolledWindow::builder()
            .min_content_height(120)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build();
        content.append(&scroll);
        let buttons = flow();
        let dashboard = gtk::Button::with_label("Buka dashboard");
        let weak = parent.downgrade();
        dashboard.connect_clicked(move |_| {
            if let Some(parent) = weak.upgrade() {
                parent.present();
            }
        });
        let close = gtk::Button::with_label("Tutup");
        let weak = window.downgrade();
        close.connect_clicked(move |_| {
            if let Some(window) = weak.upgrade() {
                window.set_visible(false);
            }
        });
        dashboard.add_css_class("suggested-action");
        buttons.insert(&dashboard, -1);
        buttons.insert(&close, -1);
        content.append(&buttons);
        window.set_child(Some(&content));
        Self {
            window,
            status,
            summary,
            bar,
            list,
            known: RefCell::new(HashSet::new()),
            initialized: Cell::new(false),
            signature: RefCell::new(String::new()),
            last_notified: Cell::new(0),
            snapshot_mode,
            pending_count: Cell::new(0),
            retry_count: Cell::new(0),
            delete_count: Cell::new(0),
        }
    }
    /// Update service health even when the database activity list cannot be read.
    pub fn refresh_status(&self, snapshot: &Snapshot) {
        let active = snapshot.live
            && snapshot
                .status
                .as_ref()
                .is_some_and(|s| s.progress.has_file_activity(mygdrive_sync::now_ms()));
        let message = if let Some(notice) = snapshot.notice() {
            notice.description()
        } else if snapshot.status.as_ref().is_some_and(|s| s.paused) {
            "Sinkronisasi dijeda. Perubahan tetap tersimpan."
        } else if active {
            "Sedang menyinkronkan perubahan file…"
        } else if self.retry_count.get() > 0 {
            "Ada operasi tertunda; menunggu percobaan ulang otomatis."
        } else if self.pending_count.get() > 0 {
            "Menunggu giliran sinkronisasi…"
        } else {
            "Tidak ada transfer yang sedang berjalan."
        };
        self.status.set_text(message);
        self.summary.set_text(&format!(
            "Menunggu: {} file  ·  Penghapusan: {}  ·  Dicoba ulang: {}",
            self.pending_count.get(),
            self.delete_count.get(),
            self.retry_count.get()
        ));
        self.bar.set_visible(active);
        if active {
            self.bar.pulse();
        }
    }
    pub fn update(&self, page: &ActivityPage, snapshot: &Snapshot) {
        let items = &page.items;
        let count_increased = page.pending_files > self.pending_count.get();
        self.pending_count.set(page.pending_files);
        self.retry_count.set(page.retry_operations);
        self.delete_count.set(page.pending_deletions);
        self.refresh_status(snapshot);
        let signature = format!(
            "{:?}",
            items
                .iter()
                .map(|i| (&i.id, &i.path, &i.detail))
                .collect::<Vec<_>>()
        );
        if *self.signature.borrow() != signature {
            while let Some(child) = self.list.first_child() {
                self.list.remove(&child);
            }
            if items.is_empty() {
                self.list.append(&label(
                    "Belum ada aktivitas file. Riwayat akan muncul setelah file disinkronkan.",
                    "notice",
                ));
            }
            for item in items {
                let card = column(4);
                card.add_css_class("card");
                let heading = row();
                let name = label(&item.path, "detail");
                name.set_hexpand(true);
                name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                name.set_tooltip_text(Some(&item.path));
                heading.append(&name);
                let badge = label(
                    if item.retrying {
                        "Dicoba ulang"
                    } else if item.pending {
                        "Menunggu"
                    } else {
                        "Selesai"
                    },
                    "pill",
                );
                badge.set_valign(gtk::Align::Start);
                if item.pending {
                    badge.add_css_class("warning");
                }
                heading.append(&badge);
                card.append(&heading);
                let detail = label(&format!("{} · {}", item.operation, item.detail), "subtitle");
                detail.set_wrap(true);
                card.append(&detail);
                self.list.append(&card);
            }
            *self.signature.borrow_mut() = signature;
        }
        let new_work =
            count_increased || should_announce(items, &self.known.borrow(), self.initialized.get());
        let now = mygdrive_sync::now_ms();
        let available = snapshot.live
            && snapshot
                .status
                .as_ref()
                .is_some_and(|s| !s.paused && !s.shutdown_requested);
        if new_work
            && available
            && !self.snapshot_mode
            && !self.window.is_visible()
            && now - self.last_notified.get() > 10_000
        {
            self.last_notified.set(now);
            if let Some(app) = self.window.application() {
                let notification = gio::Notification::new("MyGdrive Rust — sinkronisasi");
                notification.set_body(Some(if page.pending_files == 0 {
                    "Sinkronisasi file selesai."
                } else {
                    "Perubahan Drive sedang diproses bertahap."
                }));
                notification.add_button("Lihat detail", "app.progress");
                app.send_notification(Some("sync-progress"), &notification);
            }
        }
        *self.known.borrow_mut() = items.iter().map(|i| i.id.clone()).collect();
        self.initialized.set(true);
    }
}

fn should_announce(items: &[ActivityItem], known: &HashSet<String>, initialized: bool) -> bool {
    items
        .iter()
        .any(|item| !known.contains(&item.id) && (item.pending || initialized))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_history_is_quiet_but_fast_completed_transfers_still_notify() {
        let mut items = vec![ActivityItem {
            id: "operation".into(),
            path: "file.txt".into(),
            operation: "Unggah".into(),
            detail: "Selesai".into(),
            pending: false,
            retrying: false,
        }];
        let known = HashSet::new();
        assert!(!should_announce(&items, &known, false));
        assert!(should_announce(&items, &known, true));
        let known = HashSet::from(["operation".into()]);
        assert!(!should_announce(&items, &known, true));
        items[0].pending = true;
        assert!(should_announce(&items, &HashSet::new(), false));
    }
}
