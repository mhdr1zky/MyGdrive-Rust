//! File-based desktop translations. Catalogs are loaded once at process startup.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::OnceLock,
};

static CATALOG: OnceLock<Catalog> = OnceLock::new();
const ENGLISH: &str = include_str!("../assets/locales/en.json");
const INDONESIAN: &str = include_str!("../assets/locales/id.json");
const CHINESE: &str = include_str!("../assets/locales/zh.json");

/// Normalize POSIX and BCP 47 locales to a language file name. Other language
/// codes are accepted so a user can supply a new catalog without recompiling.
pub fn language_code(locale: &str) -> Option<String> {
    let locale = locale
        .trim()
        .split(['.', '@'])
        .next()?
        .replace('_', "-")
        .to_ascii_lowercase();
    if locale == "c" || locale == "posix" {
        return Some("en".into());
    }
    let code = locale.split('-').next()?;
    if (2..=3).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphabetic()) {
        Some(code.to_owned())
    } else {
        None
    }
}

/// LC_ALL overrides LC_MESSAGES, which overrides LANG. GNU LANGUAGE can choose
/// message languages except under the C/POSIX locale.
fn system_languages(get: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| get(name).filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "en".into());
    let base = locale.split(['.', '@']).next().unwrap_or("");
    if base.eq_ignore_ascii_case("c") || base.eq_ignore_ascii_case("posix") {
        return vec!["en".into()];
    }
    let mut languages: Vec<_> = get("LANGUAGE")
        .unwrap_or_default()
        .split(':')
        .filter_map(language_code)
        .collect();
    if let Some(code) = language_code(&locale) {
        languages.push(code);
    }
    languages.push("en".into());
    languages
}

fn catalog_dirs() -> Vec<PathBuf> {
    // Later locations override earlier locations, key by key.
    let mut dirs = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/locales")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(root) = exe.parent().and_then(Path::parent) {
            dirs.push(root.join("share/mygdrive/locales")); // AppImage
            dirs.push(root.join("assets/locales")); // Debian
        }
    }
    if let Some(base) = directories::BaseDirs::new() {
        dirs.push(base.config_dir().join("mygdrive/locales"));
    }
    if let Some(dir) = std::env::var_os("MYGDRIVE_LOCALES_DIR") {
        dirs.push(dir.into());
    }
    dirs
}

struct Catalog {
    language: String,
    messages: BTreeMap<String, String>,
}
fn placeholders(text: &str) -> Option<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    let mut cursor = text;
    let mut index = 0;
    while let Some(start) = cursor.find('{') {
        if cursor[..start].contains('}') {
            return None;
        }
        let end = cursor[start + 1..].find('}')? + start + 1;
        let name = &cursor[start + 1..end];
        if name.is_empty() {
            names.insert(index.to_string());
            index += 1;
        } else if name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            names.insert(name.into());
        } else {
            return None;
        }
        cursor = &cursor[end + 1..];
    }
    if cursor.contains('}') {
        return None;
    }
    Some(names)
}
impl Catalog {
    fn load(languages: &[String], dirs: &[PathBuf]) -> Self {
        let english: BTreeMap<String, String> =
            serde_json::from_str(ENGLISH).expect("bundled English catalog");
        let mut messages = english.clone();
        let mut selected = "en".to_owned();
        // Resolve the first available language, falling back to English per key.
        for language in languages {
            let builtin = match language.as_str() {
                "en" => Some(ENGLISH),
                "id" => Some(INDONESIAN),
                "zh" => Some(CHINESE),
                _ => None,
            };
            let mut available = builtin.is_some();
            let mut overlay =
                |text: &str| match serde_json::from_str::<BTreeMap<String, String>>(text) {
                    Ok(entries) => {
                        for (key, value) in entries {
                            if let Some(fallback) = english.get(&key) {
                                if !value.trim().is_empty()
                                    && placeholders(&value) == placeholders(fallback)
                                {
                                    messages.insert(key, value);
                                } else {
                                    eprintln!(
                                        "Ignoring invalid translation in {language}.json: {key}"
                                    );
                                }
                            }
                        }
                        true
                    }
                    Err(error) => {
                        eprintln!("Ignoring invalid {language}.json: {error}");
                        false
                    }
                };
            if let Some(text) = builtin {
                overlay(text);
            }
            for dir in dirs {
                if let Ok(text) = std::fs::read_to_string(dir.join(format!("{language}.json"))) {
                    available |= overlay(&text);
                }
            }
            if available {
                selected = language.clone();
                break;
            }
        }
        Self {
            language: selected,
            messages,
        }
    }
    fn text<'a>(&'a self, key: &'a str) -> &'a str {
        self.messages.get(key).map(String::as_str).unwrap_or(key)
    }
}

fn catalog() -> &'static Catalog {
    CATALOG.get_or_init(|| {
        let explicit = std::env::var("MYGDRIVE_LANGUAGE")
            .ok()
            .filter(|v| v != "auto");
        let languages = explicit
            .and_then(|v| language_code(&v))
            .map(|v| vec![v, "en".into()])
            .unwrap_or_else(|| system_languages(|name| std::env::var(name).ok()));
        Catalog::load(&languages, &catalog_dirs())
    })
}

/// Initialize before creating the GTK application or worker threads.
pub fn initialize(language: Option<&str>) {
    let languages = match language {
        Some("auto") => system_languages(|name| std::env::var(name).ok()),
        Some(value) => language_code(value)
            .map(|code| vec![code, "en".into()])
            .unwrap_or_else(|| vec!["en".into()]),
        None => {
            let _ = catalog();
            return;
        }
    };
    let _ = CATALOG.set(Catalog::load(&languages, &catalog_dirs()));
}
pub fn current_language() -> &'static str {
    &catalog().language
}
pub fn tr(key: &'static str) -> &'static str {
    catalog().text(key)
}

/// Interpolate in one pass so filenames containing braces cannot be interpreted
/// as placeholders. Translators may reorder values using {0}, {1}, etc.
pub fn fmt(key: &'static str, values: &[(&str, String)]) -> String {
    interpolate(tr(key), values)
}
fn interpolate(template: &str, values: &[(&str, String)]) -> String {
    let mut result = String::new();
    let mut cursor = template;
    let mut index = 0;
    while let Some(start) = cursor.find('{') {
        let Some(end) = cursor[start + 1..].find('}').map(|i| i + start + 1) else {
            break;
        };
        result.push_str(&cursor[..start]);
        let placeholder = &cursor[start + 1..end];
        let automatic;
        let name = if placeholder.is_empty() {
            automatic = index.to_string();
            index += 1;
            automatic.as_str()
        } else {
            placeholder
        };
        if let Some((_, value)) = values.iter().find(|(key, _)| *key == name) {
            result.push_str(value);
        } else {
            result.push_str(&cursor[start..=end]);
        }
        cursor = &cursor[end + 1..];
    }
    result.push_str(cursor);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locale_precedence_and_aliases() {
        let detect = |pairs: &[(&str, &str)]| {
            system_languages(|name| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            })
        };
        assert_eq!(detect(&[("LANG", "id_ID.UTF-8")])[0], "id");
        assert_eq!(
            detect(&[("LC_ALL", "zh_CN.UTF-8"), ("LANG", "id_ID")])[0],
            "zh"
        );
        assert_eq!(
            detect(&[("LC_MESSAGES", "en_GB"), ("LANG", "id_ID")])[0],
            "en"
        );
        assert_eq!(
            detect(&[("LANG", "id_ID"), ("LANGUAGE", "zh_CN:en")])[0],
            "zh"
        );
        assert_eq!(detect(&[("LANG", "C.UTF-8"), ("LANGUAGE", "id")]), ["en"]);
        assert_eq!(language_code("../../etc"), None);
        assert_eq!(language_code("zh-Hans-CN"), Some("zh".into()));
    }
    #[test]
    fn bundled_catalogs_have_identical_keys_and_placeholders() {
        let english: BTreeMap<String, String> = serde_json::from_str(ENGLISH).unwrap();
        for data in [INDONESIAN, CHINESE] {
            let other: BTreeMap<String, String> = serde_json::from_str(data).unwrap();
            assert_eq!(
                english.keys().collect::<Vec<_>>(),
                other.keys().collect::<Vec<_>>()
            );
            for (key, value) in &other {
                assert!(!value.trim().is_empty(), "{key}");
                assert_eq!(placeholders(value), placeholders(&english[key]), "{key}");
            }
        }
        assert_eq!(Catalog::load(&["id".into()], &[]).text("Jeda"), "Jeda");
        assert_eq!(Catalog::load(&["zh".into()], &[]).text("Jeda"), "暂停");
        assert_eq!(
            Catalog::load(&["xx".into(), "en".into()], &[]).text("Jeda"),
            "Pause"
        );
    }
    #[test]
    fn runtime_files_and_invalid_overrides_fall_back_safely() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("id.json"),
            r#"{"Jeda":"Jeda kustom","Interval sinkronisasi: {} detik":"Rusak {missing}"}"#,
        )
        .unwrap();
        let catalog = Catalog::load(&["id".into()], &[dir.path().into()]);
        assert_eq!(catalog.text("Jeda"), "Jeda kustom");
        assert_eq!(
            catalog.text("Interval sinkronisasi: {} detik"),
            "Interval sinkronisasi: {} detik"
        );
        std::fs::write(dir.path().join("id.json"), "invalid JSON").unwrap();
        assert_eq!(
            Catalog::load(&["id".into()], &[dir.path().into()]).text("Jeda"),
            "Jeda"
        );
        std::fs::write(dir.path().join("fr.json"), r#"{"Jeda":"Pause FR"}"#).unwrap();
        let catalog = Catalog::load(&["fr".into()], &[dir.path().into()]);
        assert_eq!(catalog.text("Jeda"), "Pause FR");
        assert_eq!(catalog.text("Lanjutkan"), "Resume");
    }
    #[test]
    fn formatting_can_reorder_values_without_reinterpreting_user_content() {
        let args = &[("0", "a{1}.txt".into()), ("1", "42".into())];
        assert_eq!(interpolate("{1} · {0}", args), "42 · a{1}.txt");
        assert_eq!(interpolate("{} · {}", args), "a{1}.txt · 42");
    }
}
