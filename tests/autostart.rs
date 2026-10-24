use mygdrive_sync::autostart::Autostart;
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
};

#[test]
fn profile_autostart_is_opt_in_private_independent_and_preserves_unmanaged_entries() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config");
    let state = temp.path().join("state");
    let second = temp.path().join("second");
    fs::create_dir(&state).unwrap();
    fs::create_dir(&second).unwrap();
    let launcher = temp.path().join("my drive $`%\\\" launcher");
    fs::write(&launcher, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let first = Autostart::new(&config, &state, &launcher).unwrap();
    let other = Autostart::new(&config, &second, &launcher).unwrap();
    assert!(!first.enabled().unwrap());
    assert!(!config.exists());
    first.set_enabled(true).unwrap();
    other.set_enabled(true).unwrap();
    assert_ne!(first.path(), other.path());
    assert!(first.enabled().unwrap());
    let original = fs::read(first.path()).unwrap();
    first.set_enabled(true).unwrap();
    assert_eq!(fs::read(first.path()).unwrap(), original);
    assert_eq!(
        fs::metadata(first.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Ask the system desktop-entry validator to validate our escaping where available.
    if let Ok(result) = std::process::Command::new("desktop-file-validate")
        .arg(first.path())
        .output()
    {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
    }
    first.set_enabled(false).unwrap();
    assert!(!first.enabled().unwrap());
    assert!(other.enabled().unwrap());
    fs::write(
        first.path(),
        "[Desktop Entry]\nType=Application\nName=Another app\nExec=/bin/true\n",
    )
    .unwrap();
    let foreign = fs::read(first.path()).unwrap();
    assert!(first.set_enabled(true).is_err());
    assert!(first.set_enabled(false).is_err());
    assert_eq!(fs::read(first.path()).unwrap(), foreign);
}

#[test]
fn autostart_rejects_symlink_replacement_and_control_characters() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config");
    let state = temp.path().join("state");
    fs::create_dir(&state).unwrap();
    let entry = Autostart::new(&config, &state, std::path::Path::new("/bin/true")).unwrap();
    fs::create_dir_all(config.join("autostart")).unwrap();
    let target = temp.path().join("do-not-change");
    fs::write(&target, "keep").unwrap();
    symlink(&target, entry.path()).unwrap();
    assert!(entry.set_enabled(true).is_err());
    assert!(entry.set_enabled(false).is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    assert!(Autostart::new(
        &config,
        &state,
        std::path::Path::new("/tmp/launcher\nExec=bad")
    )
    .is_err());
}

#[cfg(feature = "desktop-ui")]
#[test]
fn desktop_exec_escaping_round_trips_through_gio_without_shell_expansion() {
    // Use the installed GIO desktop launcher, not an imitation of the Exec parser.
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config");
    let state = temp.path().join("state $`%\\\" spaces");
    fs::create_dir(&state).unwrap();
    let launcher = temp.path().join("launcher $`%\\\" spaces");
    let output = temp.path().join("argv");
    fs::write(&launcher,format!("#!/usr/bin/python3\nimport json,sys\nwith open({:?},'w') as f: json.dump(sys.argv[1:],f)\n",output.to_str().unwrap())).unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let entry = Autostart::new(&config, &state, &launcher).unwrap();
    entry.set_enabled(true).unwrap();
    let result = std::process::Command::new("gio")
        .arg("launch")
        .arg(entry.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    for _ in 0..100 {
        if output.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let args: Vec<String> = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    assert_eq!(
        args,
        vec![
            "--state",
            state.to_str().unwrap(),
            "--background",
            "--start-service"
        ]
    );
}
