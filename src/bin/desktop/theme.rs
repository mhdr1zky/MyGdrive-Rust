use gtk::{gio, prelude::*};

const INTERFACE_SCHEMA: &str = "org.gnome.desktop.interface";
const COLOR_SCHEME: &str = "color-scheme";
const PREFER_DARK: &str = "gtk-application-prefer-dark-theme";

/// Keep GTK's fallback theme in the system's light/dark variant, without
/// replacing the selected native theme. Some desktop themes support GTK 3 only.
pub fn install() {
    let Some(settings) = gtk::Settings::default() else {
        return;
    };
    let appearance = gio::SettingsSchemaSource::default()
        .and_then(|source| source.lookup(INTERFACE_SCHEMA, true))
        .filter(|schema| schema.has_key(COLOR_SCHEME))
        .map(|schema| gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, None));

    if let Some(appearance) = &appearance {
        let weak_settings = settings.downgrade();
        appearance.connect_changed(Some(COLOR_SCHEME), move |appearance, _| {
            if let Some(settings) = weak_settings.upgrade() {
                apply(&settings, Some(appearance));
            }
        });
    }

    apply(&settings, appearance.as_ref());
    // This closure retains the optional settings object for GTK's lifetime.
    settings.connect_gtk_theme_name_notify(move |settings| {
        apply(settings, appearance.as_ref());
    });
}

fn apply(settings: &gtk::Settings, appearance: Option<&gio::Settings>) {
    let theme_name = settings.gtk_theme_name();
    let color_scheme = appearance.map(|appearance| appearance.string(COLOR_SCHEME));
    match preferred_dark(theme_name.as_deref(), color_scheme.as_deref()) {
        Some(dark) => settings.set_gtk_application_prefer_dark_theme(dark),
        None => settings.reset_property(PREFER_DARK),
    }
}

fn preferred_dark(theme_name: Option<&str>, color_scheme: Option<&str>) -> Option<bool> {
    let name = theme_name.unwrap_or("Default");
    for token in name.split(|character: char| !character.is_alphanumeric()) {
        if token.eq_ignore_ascii_case("dark") {
            return Some(true);
        }
        if token.eq_ignore_ascii_case("light") {
            return Some(false);
        }
    }

    // Traditional desktops select a named GTK theme directly. A separate GNOME
    // appearance preference can be stale there, so keep the native default for
    // custom names (including the light Mint-L counterpart of Mint-L-Dark).
    if !name.is_empty()
        && !name.eq_ignore_ascii_case("Default")
        && !name.eq_ignore_ascii_case("Adwaita")
    {
        return None;
    }

    match color_scheme {
        Some("prefer-dark") => Some(true),
        Some("prefer-light") => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::preferred_dark;

    #[test]
    fn selected_native_theme_takes_priority_over_stale_appearance_preference() {
        assert_eq!(
            preferred_dark(Some("Mint-L-Dark"), Some("prefer-light")),
            Some(true)
        );
        assert_eq!(preferred_dark(Some("Mint-Y-Dark-Aqua"), None), Some(true));
        assert_eq!(
            preferred_dark(Some("Theme_LiGhT"), Some("prefer-dark")),
            Some(false)
        );
        assert_eq!(preferred_dark(Some("Mint-L"), Some("prefer-dark")), None);
        assert_eq!(preferred_dark(Some("Custom"), Some("prefer-dark")), None);
    }

    #[test]
    fn default_gtk_themes_follow_the_system_appearance_preference() {
        assert_eq!(
            preferred_dark(Some("Adwaita"), Some("prefer-dark")),
            Some(true)
        );
        assert_eq!(
            preferred_dark(Some("Default"), Some("prefer-light")),
            Some(false)
        );
        assert_eq!(preferred_dark(None, Some("prefer-dark")), Some(true));
        assert_eq!(preferred_dark(Some("Adwaita"), Some("default")), None);
        assert_eq!(preferred_dark(Some("Default"), None), None);
    }
}
