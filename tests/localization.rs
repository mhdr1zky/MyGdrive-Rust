//! Separate processes verify locale detection without racing global environment.
use std::process::Command;

#[test]
fn language_probe() {
    let Ok(expected) = std::env::var("MYGDRIVE_TEST_EXPECTED_LANGUAGE") else {
        return;
    };
    let selection = std::env::var("MYGDRIVE_TEST_LANGUAGE_SELECTION").ok();
    mygdrive_sync::i18n::initialize(selection.as_deref());
    assert_eq!(mygdrive_sync::i18n::current_language(), expected);
    assert_eq!(
        mygdrive_sync::i18n::tr("Jeda"),
        std::env::var("MYGDRIVE_TEST_EXPECTED_PAUSE").unwrap()
    );
}

#[test]
fn locale_and_explicit_overrides_work_in_fresh_processes() {
    fn probe(vars: &[(&str, &str)], language: &str, pause: &str) {
        let mut child = Command::new(std::env::current_exe().unwrap());
        for name in [
            "LANG",
            "LANGUAGE",
            "LC_ALL",
            "LC_MESSAGES",
            "MYGDRIVE_LANGUAGE",
            "MYGDRIVE_LOCALES_DIR",
            "MYGDRIVE_TEST_LANGUAGE_SELECTION",
        ] {
            child.env_remove(name);
        }
        let config = tempfile::tempdir().unwrap();
        child
            .env("XDG_CONFIG_HOME", config.path())
            .env("MYGDRIVE_TEST_EXPECTED_LANGUAGE", language)
            .env("MYGDRIVE_TEST_EXPECTED_PAUSE", pause)
            .args(["--exact", "language_probe", "--nocapture"]);
        for (key, value) in vars {
            child.env(key, value);
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    probe(&[("LANG", "en_US.UTF-8")], "en", "Pause");
    probe(&[("LANG", "id_ID.UTF-8")], "id", "Jeda");
    probe(&[("LANG", "zh_CN.UTF-8")], "zh", "暂停");
    probe(&[("LANG", "de_DE.UTF-8")], "en", "Pause");
    probe(&[("LANG", "id_ID"), ("LC_MESSAGES", "zh_CN")], "zh", "暂停");
    probe(&[("LANG", "id_ID"), ("LANGUAGE", "zh:en")], "zh", "暂停");
    probe(
        &[("LANG", "id_ID"), ("LC_ALL", "C.UTF-8"), ("LANGUAGE", "zh")],
        "en",
        "Pause",
    );
    probe(
        &[("LANG", "zh_CN"), ("MYGDRIVE_LANGUAGE", "id")],
        "id",
        "Jeda",
    );
    probe(
        &[
            ("LANG", "zh_CN"),
            ("MYGDRIVE_LANGUAGE", "id"),
            ("MYGDRIVE_TEST_LANGUAGE_SELECTION", "auto"),
        ],
        "zh",
        "暂停",
    );
    probe(
        &[
            ("LANG", "zh_CN"),
            ("MYGDRIVE_LANGUAGE", "id"),
            ("MYGDRIVE_TEST_LANGUAGE_SELECTION", "en"),
        ],
        "en",
        "Pause",
    );
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("id.json"),
        r#"{"Jeda":"Custom pause"}"#,
    )
    .unwrap();
    probe(
        &[
            ("LANG", "id_ID"),
            ("MYGDRIVE_LOCALES_DIR", directory.path().to_str().unwrap()),
        ],
        "id",
        "Custom pause",
    );
}
