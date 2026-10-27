use fs2::FileExt;
use mygdrive_sync::{account_manager, dashboard, oauth, service};
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
};

fn private_json(path: &Path, value: &impl serde::Serialize) -> std::io::Result<()> {
    fs::write(path, serde_json::to_vec(value).unwrap())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

fn profile(base: &Path, name: &str, email: &str) -> PathBuf {
    let state = base.join("profiles").join(name);
    let root = base.join(format!("local-{name}"));
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&root).unwrap();
    private_json(
        &service::config_path(&state),
        &service::ServiceConfig {
            version: 1,
            account: "default".into(),
            root,
            drive_folder: "root".into(),
            interval_seconds: 30,
            cache_max_bytes: None,
        },
    )
    .unwrap();
    let token = oauth::token_path(&state, "default");
    fs::create_dir_all(token.parent().unwrap()).unwrap();
    fs::write(&token, serde_json::to_vec(&serde_json::json!({
        "client":{"client_id":"demo.apps.googleusercontent.com","client_secret":"synthetic"},
        "access_token":"synthetic-access","refresh_token":"synthetic-refresh","expires_at_ms":i64::MAX,
        "scope":oauth::DRIVE_SCOPE,"drive_user_id":name,"identity":{"email":email,"display_name":name}
    })).unwrap()).unwrap();
    fs::set_permissions(token, fs::Permissions::from_mode(0o600)).unwrap();
    state
}

#[tokio::test]
async fn accounts_keep_identity_and_data_isolated_after_local_disconnect() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("collection");
    let first = profile(&base, "personal", "a@example.test");
    let second = profile(&base, "work", "b@example.test");
    let second_before = fs::read(oauth::token_path(&second, "default")).unwrap();
    let marker = base.join("local-personal/document.txt");
    fs::write(&marker, b"local edits").unwrap();
    oauth::disconnect_local(&oauth::token_path(&first, "default")).unwrap();
    let rows = account_manager::list(&base).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].email(), "a@example.test");
    assert!(rows[0].signed_out && rows[0].configured);
    assert!(!rows[1].signed_out);
    assert_eq!(fs::read(&marker).unwrap(), b"local edits");
    assert_eq!(
        fs::read(oauth::token_path(&second, "default")).unwrap(),
        second_before
    );
    assert_eq!(
        account_manager::validate_target(&base, &first).unwrap(),
        first
    );
    assert_eq!(account_manager::base_for(&first), base);
}

#[tokio::test]
async fn outside_profiles_symlinks_and_unfinished_registration_cannot_be_controlled() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("collection");
    let first = profile(&base, "first", "first@example.test");
    let outside = profile(
        &temp.path().join("other"),
        "outside",
        "outside@example.test",
    );
    symlink(&outside, base.join("profiles/linked")).unwrap();
    let draft = base.join("profiles/draft");
    fs::create_dir(&draft).unwrap();
    oauth::prepare_new_account(
        &oauth::token_path(&first, "default"),
        &oauth::token_path(&draft, "default"),
    )
    .unwrap();
    assert_eq!(account_manager::list(&base).await.unwrap().len(), 1);
    for target in [&outside, &base.join("profiles/linked"), &draft] {
        assert!(account_manager::control(
            &base,
            target,
            Path::new("/does-not-exist"),
            dashboard::Command::Pause
        )
        .await
        .is_err());
    }
}

#[tokio::test]
async fn update_receipt_preserves_interrupted_drains_and_validates_before_starting() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("collection");
    let first = profile(&base, "first", "first@example.test");
    let lock = fs::File::create(first.join("engine.lock")).unwrap();
    lock.lock_exclusive().unwrap();
    // An unresponsive daemon still owns the engine; an update cannot treat it as stopped.
    assert!(
        account_manager::drain_collection(&base, Path::new("/unused"), true)
            .await
            .is_err()
    );
    let receipt = base.join("restart-accounts.json");
    let record: serde_json::Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    assert_eq!(record["profiles"][0], first.to_str().unwrap());
    drop(lock);
    account_manager::drain_collection(&base, Path::new("/unused"), true)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&receipt).unwrap()).unwrap(),
        record
    );
    let outside = profile(&temp.path().join("other"), "outside", "other@example.test");
    private_json(
        &receipt,
        &serde_json::json!({"version":1,"profiles":[first,outside]}),
    )
    .unwrap();
    let launcher = temp.path().join("launcher");
    let marker = temp.path().join("started");
    fs::write(
        &launcher,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(account_manager::restore_collection(&base, &launcher)
        .await
        .is_err());
    assert!(!marker.exists());
    assert!(receipt.exists());
}

#[tokio::test]
async fn signed_out_accounts_are_not_restarted_and_receipt_is_consumed() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("collection");
    let first = profile(&base, "first", "first@example.test");
    oauth::disconnect_local(&oauth::token_path(&first, "default")).unwrap();
    private_json(
        &base.join("restart-accounts.json"),
        &serde_json::json!({"version":1,"profiles":[first]}),
    )
    .unwrap();
    account_manager::restore_collection(&base, Path::new("/does-not-exist"))
        .await
        .unwrap();
    assert!(!base.join("restart-accounts.json").exists());
}
