use async_trait::async_trait;
use mygdrive_sync::{
    drive::AccessTokenProvider,
    onboarding::{self, Folder, FolderBrowser},
    service, Result,
};
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    sync::Arc,
};
use wiremock::{
    matchers::{header, path, query_param, query_param_is_missing},
    Mock, MockServer, ResponseTemplate,
};

struct Token;
#[async_trait]
impl AccessTokenProvider for Token {
    async fn access_token(&self) -> Result<String> {
        Ok("mock-token".into())
    }
}

#[tokio::test]
async fn folder_browser_filters_parent_and_preserves_pagination() {
    let server = MockServer::start().await;
    Mock::given(path("/files")).and(header("authorization","Bearer mock-token"))
        .and(query_param("q","'root' in parents and mimeType = 'application/vnd.google-apps.folder' and trashed = false"))
        .and(query_param("fields","nextPageToken,files(id,name)")).and(query_param_is_missing("pageToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"files":[],"nextPageToken":"p+2/="}))).expect(1).mount(&server).await;
    Mock::given(path("/files")).and(query_param("pageToken","p+2/="))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"files":[{"id":"f1","name":"Same name"},{"id":"f2","name":"Same name"}]}))).expect(1).mount(&server).await;
    let client =
        FolderBrowser::with_endpoint(Arc::new(Token), &format!("{}/files", server.uri())).unwrap();
    let first = client.list("root", None).await.unwrap();
    assert!(first.files.is_empty());
    let second = client.list("root", first.next.as_deref()).await.unwrap();
    assert_eq!(second.files.len(), 2);
    assert_ne!(second.files[0].id, second.files[1].id);
    assert!(second.next.is_none());
    assert!(client.list("root' or trashed=true", None).await.is_err());
}

#[tokio::test]
async fn folder_validation_rejects_deleted_files_and_shared_drives_and_sanitizes_errors() {
    let server = MockServer::start().await;
    for (id, extra) in [
        ("deleted", serde_json::json!({"trashed":true})),
        ("shared", serde_json::json!({"driveId":"shared-drive"})),
        ("file", serde_json::json!({"mimeType":"text/plain"})),
    ] {
        let mut body = serde_json::json!({"id":id,"name":"Folder","mimeType":"application/vnd.google-apps.folder"});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        Mock::given(path(format!("/files/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    Mock::given(path("/files/root")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":"actual-root","name":"My Drive","mimeType":"application/vnd.google-apps.folder"}))).mount(&server).await;
    Mock::given(path("/files/denied"))
        .respond_with(ResponseTemplate::new(403).set_body_string("secret token server error"))
        .mount(&server)
        .await;
    let client =
        FolderBrowser::with_endpoint(Arc::new(Token), &format!("{}/files", server.uri())).unwrap();
    for id in ["deleted", "shared", "file"] {
        assert!(client.validate_folder(id).await.is_err());
    }
    assert_eq!(
        client.validate_folder("root").await.unwrap().id,
        "actual-root"
    );
    let error = client
        .validate_folder("denied")
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("secret"));
    assert!(error.contains("403"));
    assert!(FolderBrowser::with_endpoint(Arc::new(Token), "http://example.com/files").is_err());
}

#[test]
fn pair_configuration_is_private_never_overwrites_and_rejects_overlapping_roots() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("state");
    let state = base.join("profiles/new");
    fs::create_dir_all(&state).unwrap();
    let root = temp.path().join("local");
    fs::create_dir(&root).unwrap();
    let folder = Folder {
        id: "remote-folder".into(),
        name: "Folder".into(),
    };
    let config = onboarding::save_pair(&base, &state, &root, &folder).unwrap();
    assert_eq!(config.root, root.canonicalize().unwrap());
    assert!(!state.join("state.sqlite3").exists());
    assert!(!state.join("service-status.json").exists());
    assert_eq!(
        fs::metadata(service::config_path(&state))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let original = fs::read(service::config_path(&state)).unwrap();
    let other_root = temp.path().join("other");
    fs::create_dir(&other_root).unwrap();
    assert!(onboarding::save_pair(&base, &state, &other_root, &folder).is_err());
    assert_eq!(fs::read(service::config_path(&state)).unwrap(), original);
    let second = base.join("profiles/second");
    fs::create_dir(&second).unwrap();
    let alias = temp.path().join("alias");
    symlink(&root, &alias).unwrap();
    assert!(onboarding::save_pair(&base, &second, &alias, &folder).is_err());
    let nested = root.join("nested");
    fs::create_dir(&nested).unwrap();
    assert!(onboarding::save_pair(&base, &second, &nested, &folder).is_err());
    assert!(onboarding::save_pair(&base, &second, temp.path(), &folder).is_err());
    fs::write(other_root.join("existing.txt"), "existing").unwrap();
    assert!(onboarding::save_pair(&base, &second, &other_root, &folder).is_err());
    assert_eq!(onboarding::profiles(&base).unwrap().len(), 1);
}

#[test]
fn concurrent_profile_saves_cannot_claim_the_same_local_root() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("state");
    let root = temp.path().join("local");
    fs::create_dir(&root).unwrap();
    // Pre-create private directories: the race under test is publication, not mkdir.
    mygdrive_sync::local::private_dir(&base).unwrap();
    mygdrive_sync::local::private_dir(&base.join("profiles")).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut workers = Vec::new();
    for id in ["a", "b"] {
        let state = base.join("profiles").join(id);
        fs::create_dir(&state).unwrap();
        let base = base.clone();
        let root = root.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            onboarding::save_pair(
                &base,
                &state,
                &root,
                &Folder {
                    id: "remote".into(),
                    name: "Folder".into(),
                },
            )
            .is_ok()
        }));
    }
    assert_eq!(
        workers
            .into_iter()
            .map(|w| usize::from(w.join().unwrap()))
            .sum::<usize>(),
        1
    );
    assert_eq!(onboarding::profiles(&base).unwrap().len(), 1);
}

#[test]
fn corrupt_existing_config_blocks_new_pair_instead_of_hiding_root_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("state");
    let draft = base.join("profiles/draft");
    fs::create_dir_all(&draft).unwrap();
    fs::write(service::config_path(&base), b"damaged configuration").unwrap();
    let root = temp.path().join("local");
    fs::create_dir(&root).unwrap();
    assert!(onboarding::profiles(&base).is_err());
    assert!(onboarding::save_pair(
        &base,
        &draft,
        &root,
        &Folder {
            id: "folder".into(),
            name: "Folder".into()
        }
    )
    .is_err());
    assert!(!service::config_path(&draft).exists());
}

#[tokio::test]
#[ignore = "explicit opt-in, reads user's Drive folder metadata using existing OAuth"]
async fn live_folder_browser() {
    let state = std::path::PathBuf::from(
        std::env::var("MYGDRIVE_LIVE_STATE").expect("explicit state required"),
    );
    let expected =
        std::env::var("MYGDRIVE_EXPECT_FOLDER").expect("explicit expected folder required");
    let provider = mygdrive_sync::oauth::OAuthProvider::new(mygdrive_sync::oauth::token_path(
        &state, "default",
    ))
    .unwrap();
    let client = FolderBrowser::new(Arc::new(provider)).unwrap();
    assert_eq!(
        client.validate_folder(&expected).await.unwrap().id,
        expected
    );
    let mut page = None;
    let mut found = false;
    for _ in 0..20 {
        let result = client.list("root", page.as_deref()).await.unwrap();
        found |= result.files.iter().any(|f| f.id == expected);
        page = result.next;
        if page.is_none() {
            break;
        }
    }
    assert!(found, "expected test folder must appear in My Drive");
}

#[tokio::test]
async fn reopening_setup_restores_email_target_and_authenticated_unfinished_accounts() {
    use std::time::Duration;
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("state");
    let root = temp.path().join("local");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir(&root).unwrap();
    let config = service::ServiceConfig {
        version: 1,
        account: "default".into(),
        root: root.clone(),
        drive_folder: "root".into(),
        interval_seconds: 30,
        cache_max_bytes: None,
    };
    fs::write(
        service::config_path(&base),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let draft = base.join("profiles/unfinished");
    for state in [&base, &draft] {
        let token = mygdrive_sync::oauth::token_path(state, "default");
        fs::create_dir_all(token.parent().unwrap()).unwrap();
        fs::write(&token,serde_json::to_vec(&serde_json::json!({
            "client":{"client_id":"test.apps.googleusercontent.com","client_secret":"test"},
            "access_token":"private-test-token","refresh_token":"test-refresh","scope":"https://www.googleapis.com/auth/drive",
            "expires_at_ms":i64::MAX,"drive_user_id":"owner",
            "identity":{"email":"user@example.test","display_name":"Test User"}
        })).unwrap()).unwrap();
        fs::set_permissions(token, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let worker = onboarding::spawn_observer(base.clone(), base.clone(), true).unwrap();
    let mut connected = false;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            while let Ok(event) = worker.events.try_recv() {
                if let onboarding::Event::Connected {
                    identity,
                    config,
                    folder,
                } = event
                {
                    assert_eq!(identity.unwrap().email, "user@example.test");
                    assert_eq!(config.unwrap().root, root);
                    assert_eq!(folder.unwrap().id, "root");
                    connected = true;
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(connected);
    worker.stop();
    drop(worker);
    let accounts = onboarding::accounts(&base).unwrap();
    assert_eq!(accounts.len(), 2);
    assert!(accounts
        .iter()
        .any(|a| a.state == draft && a.config.is_none()));
    assert!(
        !base.join("state.sqlite3").exists(),
        "setup must not start synchronization"
    );
}
