use mygdrive_sync::{db::Database, local::INTERNAL_PREFIX, maintenance, model::LocalFile, service};
use std::os::unix::fs::{symlink, MetadataExt};

#[test]
fn storage_accounts_for_recovery_sparse_files_and_hardlinks_without_following_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    let state = temp.path().join("state");
    for path in [
        &root,
        &state,
        &state.join("cache"),
        &state.join("outbox"),
        &state.join("recovery"),
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    let backing = root.join("document");
    std::fs::write(&backing, [1; 8192]).unwrap();
    std::fs::hard_link(&backing, root.join("same-inode")).unwrap();
    let sparse = root.join("sparse");
    std::fs::File::create(&sparse)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let swapped = root.join(format!("{INTERNAL_PREFIX}old.swap"));
    std::fs::write(&swapped, [2; 4096]).unwrap();
    let cached = state.join("cache/object.blob");
    std::fs::write(&cached, [3; 16384]).unwrap();
    std::fs::write(state.join("outbox/pending.blob"), [4; 12288]).unwrap();
    std::fs::write(state.join("recovery/backup"), [5; 4096]).unwrap();
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("never-counted"), [6; 32768]).unwrap();
    symlink(&outside, root.join("symlink")).unwrap();
    let result = maintenance::measure(&root, &state);
    let allocated = |path: &std::path::Path| std::fs::metadata(path).unwrap().blocks() * 512;
    assert_eq!(
        result.backing.bytes,
        allocated(&backing) + allocated(&sparse)
    );
    assert_eq!(result.backing.files, 2);
    assert_eq!(
        result.recovery.bytes,
        allocated(&swapped) + allocated(&state.join("recovery/backup"))
    );
    assert_eq!(result.recovery.files, 2);
    assert_eq!(result.cache.bytes, allocated(&cached));
    assert_eq!(result.outbox.files, 1);
    assert_eq!(result.skipped, 1);
    assert!(result.free_backing_bytes.is_some());
    let alias = temp.path().join("root-alias");
    symlink(&outside, &alias).unwrap();
    assert_eq!(maintenance::measure(&alias, &state).backing.files, 0);
    assert!(maintenance::folder_uri(&root, "../outside/never-counted").is_err());
    assert!(maintenance::folder_uri(&root, "symlink/never-counted").is_err());
    assert!(maintenance::folder_uri(&alias, "never-counted").is_err());
    assert!(maintenance::drive_uri("bad/id?token=secret").is_none());
}

#[tokio::test]
async fn maintenance_inventory_is_paginated_read_only_and_classifies_errors() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    let state = temp.path().join("state");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir(&state).unwrap();
    let db = Database::open(&state.join("state.sqlite3"), "test")
        .await
        .unwrap();
    db.bind("account", "test").await.unwrap();
    std::fs::write(
        service::config_path(&state),
        serde_json::to_vec(&service::ServiceConfig {
            version: 1,
            account: "test".into(),
            root: root.clone(),
            drive_folder: "root".into(),
            interval_seconds: 10,
            cache_max_bytes: None,
        })
        .unwrap(),
    )
    .unwrap();
    for n in 0..28 {
        db.save_file(&LocalFile {
            id: format!("id-{n:02}"),
            relative_path: format!("file-{n:02}.txt"),
            remote_id: None,
            generation: 1,
            fingerprint: None,
            baseline: None,
            status: "recovery_review".into(),
            materialized: true,
            review_required: true,
            available_offline: false,
            metadata_issue: (n == 0).then_some(mygdrive_sync::diagnostics::FileIssue::Quota),
        })
        .await
        .unwrap();
    }
    let mut failed = db.file("id-27").await.unwrap();
    failed.review_required = false;
    failed.status = "modified_local".into();
    db.save_file(&failed).await.unwrap();
    let mut operation = mygdrive_sync::model::Operation::new(&failed, "upload", None);
    operation.last_error = Some("transport failure".into());
    db.operation(&operation).await.unwrap();
    let before = db.files().await.unwrap().len();
    let page = maintenance::browse(&state, 0, true).await.unwrap();
    assert_eq!(page.total_issues, 28);
    assert_eq!(page.issues.len(), 25);
    assert_eq!(page.next, Some(25));
    assert!(!page.live);
    assert!(page.storage.is_some());
    assert_eq!(
        page.issues[0].diagnostic,
        Some(mygdrive_sync::diagnostics::FileIssue::Quota)
    );
    let page = maintenance::browse(&state, page.next.unwrap(), false)
        .await
        .unwrap();
    assert_eq!(page.issues.len(), 3);
    assert_eq!(page.next, None);
    assert!(page.storage.is_none());
    assert_eq!(
        page.issues[2].diagnostic,
        Some(mygdrive_sync::diagnostics::FileIssue::Connection)
    );
    operation.stage = "done".into();
    db.save_operation(&operation).await.unwrap();
    assert_eq!(
        maintenance::browse(&state, 0, false)
            .await
            .unwrap()
            .total_issues,
        27
    );
    assert_eq!(db.files().await.unwrap().len(), before);
    assert!(db.displaced_files().await.unwrap().is_empty());
}
