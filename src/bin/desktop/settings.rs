use gtk::{glib, prelude::*};
use mygdrive_sync::{
    i18n::{self, tr, Language},
    preferences,
};
use std::{cell::RefCell, rc::Rc, sync::mpsc, time::Duration};

enum Event {
    Languages(Vec<Language>),
    Saved(Result<String, ()>),
}
fn discover(events: mpsc::Sender<Event>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("mygdrive-language-discovery".into())
        .spawn(move || {
            let _ = events.send(Event::Languages(i18n::available_languages()));
        })
}
pub fn panel(
    selected_language: &str,
    snapshot_mode: bool,
    apply_language: impl Fn(String) + 'static,
) -> gtk::Box {
    let panel = super::card();
    panel.append(&super::label(tr("Interface language"), "eyebrow"));
    panel.append(&super::label(tr("Choose a language, then apply. The window reopens automatically; synchronization continues."), "subtitle"));
    let row = super::row();
    let choices = gtk::DropDown::from_strings(&[tr("Loading language files…")]);
    choices.set_enable_search(true);
    choices.set_hexpand(true);
    choices.set_sensitive(false);
    let apply = gtk::Button::with_label(tr("Apply language"));
    apply.add_css_class("suggested-action");
    apply.set_sensitive(false);
    row.append(&choices);
    row.append(&apply);
    panel.append(&row);
    let actions = super::flow();
    let reload = gtk::Button::with_label(tr("Reload languages"));
    reload.set_sensitive(false);
    actions.insert(&reload, -1);
    panel.append(&actions);
    let feedback = super::label("", "subtitle");
    panel.append(&feedback);
    let original = selected_language.to_owned();
    let languages = Rc::new(RefCell::new(Vec::<Language>::new()));
    let button = apply.clone();
    let loaded = languages.clone();
    let initial = original.clone();
    choices.connect_selected_notify(move |choices| {
        button.set_sensitive(
            loaded
                .borrow()
                .get(choices.selected() as usize)
                .is_some_and(|item| item.code != initial),
        );
    });
    let (events, results) = mpsc::channel();
    if discover(events.clone()).is_err() {
        feedback.set_text(tr(
            "Language files could not be read. Try Reload languages.",
        ));
        reload.set_sensitive(!snapshot_mode);
    }
    let events_reload = events.clone();
    let select_reload = choices.clone();
    let apply_reload = apply.clone();
    let feedback_reload = feedback.clone();
    reload.connect_clicked(move |button| {
        button.set_sensitive(false);
        select_reload.set_sensitive(false);
        apply_reload.set_sensitive(false);
        feedback_reload.set_text(tr("Loading language files…"));
        if discover(events_reload.clone()).is_err() {
            button.set_sensitive(true);
            select_reload.set_sensitive(true);
            feedback_reload.set_text(tr(
                "Language files could not be read. Try Reload languages.",
            ));
        }
    });
    let selected = choices.clone();
    let loaded = languages.clone();
    let status = feedback.clone();
    let reload_save = reload.clone();
    apply.connect_clicked(move |button| {
        let Some(language) = loaded
            .borrow()
            .get(selected.selected() as usize)
            .map(|item| item.code.clone())
        else {
            return;
        };
        let Some(directory) = preferences::directory() else {
            status.set_text(tr(
                "Language settings could not be saved. Check configuration folder permissions.",
            ));
            return;
        };
        button.set_sensitive(false);
        selected.set_sensitive(false);
        reload_save.set_sensitive(false);
        status.set_text(tr("Saving language preference…"));
        let events = events.clone();
        if std::thread::Builder::new()
            .name("mygdrive-language-settings".into())
            .spawn(move || {
                let result = preferences::save_language(&directory, &language)
                    .map(|_| language)
                    .map_err(|_| ());
                let _ = events.send(Event::Saved(result));
            })
            .is_err()
        {
            selected.set_sensitive(true);
            button.set_sensitive(true);
            reload_save.set_sensitive(true);
            status.set_text(tr(
                "Language settings could not be saved. Check configuration folder permissions.",
            ));
        }
    });
    let weak = panel.downgrade();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        while let Ok(event) = results.try_recv() {
            match event {
                Event::Languages(mut available) => {
                    let previous = languages
                        .borrow()
                        .get(choices.selected() as usize)
                        .map(|item| item.code.clone())
                        .unwrap_or_else(|| original.clone());
                    available.insert(
                        0,
                        Language {
                            code: "auto".into(),
                            name: tr("System default").into(),
                        },
                    );
                    let index = available
                        .iter()
                        .position(|item| item.code == previous)
                        .or_else(|| {
                            available
                                .iter()
                                .position(|item| item.code == i18n::current_language())
                        })
                        .unwrap_or(0);
                    let names: Vec<_> = available.iter().map(|item| item.name.as_str()).collect();
                    let model = gtk::StringList::new(&names);
                    *languages.borrow_mut() = available;
                    choices.set_model(Some(&model));
                    choices.set_selected(index as u32);
                    choices.set_sensitive(!snapshot_mode);
                    apply.set_sensitive(
                        !snapshot_mode && languages.borrow()[index].code != original,
                    );
                    reload.set_sensitive(!snapshot_mode);
                    feedback.set_text("");
                }
                Event::Saved(Ok(language)) => {
                    feedback.set_text(tr("Language saved. Reopening the application…"));
                    apply_language(language);
                }
                Event::Saved(Err(())) => {
                    choices.set_sensitive(true);
                    apply.set_sensitive(true);
                    reload.set_sensitive(true);
                    feedback.set_text(tr("Language settings could not be saved. Check configuration folder permissions."));
                }
            }
        }
        glib::ControlFlow::Continue
    });
    panel
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    fn find(root: &gtk::Widget, predicate: &impl Fn(&gtk::Widget) -> bool) -> Option<gtk::Widget> {
        if predicate(root) {
            return Some(root.clone());
        }
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Some(found) = find(&widget, predicate) {
                return Some(found);
            }
            child = widget.next_sibling();
        }
        None
    }
    fn pump_until(predicate: impl Fn() -> bool) {
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(8);
        while !predicate() {
            while context.pending() {
                context.iteration(false);
            }
            assert!(Instant::now() < deadline, "language UI did not converge");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    #[ignore = "requires a dedicated GTK display; run in an isolated desktop session"]
    fn dashboard_discovers_new_json_without_restart_and_saves_its_selection() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config");
        let locales = directory.path().join("locales");
        std::fs::create_dir(&locales).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &config);
        std::env::set_var("MYGDRIVE_LOCALES_DIR", &locales);
        mygdrive_sync::i18n::initialize(Some("en"));
        gtk::init().unwrap();
        let selected = Rc::new(RefCell::new(None));
        let result = selected.clone();
        let panel = panel("en", false, move |language| {
            *result.borrow_mut() = Some(language)
        });
        let window = gtk::Window::new();
        window.set_child(Some(&panel));
        window.present();
        let dropdown = find(panel.upcast_ref(), &|widget| widget.is::<gtk::DropDown>())
            .unwrap()
            .downcast::<gtk::DropDown>()
            .unwrap();
        let button = |caption: &str| {
            find(panel.upcast_ref(), &|widget| {
                widget
                    .clone()
                    .downcast::<gtk::Button>()
                    .is_ok_and(|button| button.label().as_deref() == Some(caption))
            })
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
        };
        let reload = button(tr("Reload languages"));
        let apply = button(tr("Apply language"));
        pump_until(|| dropdown.is_sensitive());
        assert!(!apply.is_sensitive());
        std::fs::write(
            locales.join("fr.json"),
            r#"{"_language_name":"Français","Jeda":"Pause FR"}"#,
        )
        .unwrap();
        reload.emit_clicked();
        pump_until(|| {
            dropdown.model().is_some_and(|model| {
                model.downcast::<gtk::StringList>().is_ok_and(|model| {
                    (0..model.n_items())
                        .any(|index| model.string(index).as_deref() == Some("Français"))
                })
            })
        });
        let model = dropdown
            .model()
            .unwrap()
            .downcast::<gtk::StringList>()
            .unwrap();
        let index = (0..model.n_items())
            .find(|index| model.string(*index).as_deref() == Some("Français"))
            .unwrap();
        dropdown.set_selected(index);
        assert!(apply.is_sensitive());
        apply.emit_clicked();
        pump_until(|| selected.borrow().is_some());
        assert_eq!(selected.borrow().as_deref(), Some("fr"));
        assert_eq!(
            preferences::language(&config.join("mygdrive"))
                .unwrap()
                .as_deref(),
            Some("fr")
        );
        window.destroy();
    }
}
