use mygdrive_sync::{db::Database, local::Scanner};
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::test]
async fn unchanged_scans_preserve_rows_and_still_observe_edits_moves_and_deletions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(&temp.path().join("state.sqlite3"), "test")
        .await
        .unwrap();
    let scanner = Scanner {
        root,
        db,
        gate: Arc::new(Mutex::new(())),
    };
    std::fs::write(scanner.root.join("edited.txt"), b"before").unwrap();
    std::fs::write(scanner.root.join("moved.txt"), b"move me").unwrap();
    scanner.scan().await.unwrap();
    let original = scanner.db.files().await.unwrap();
    let edited = original
        .iter()
        .find(|file| file.relative_path == "edited.txt")
        .unwrap();
    let moved = original
        .iter()
        .find(|file| file.relative_path == "moved.txt")
        .unwrap();

    scanner.scan().await.unwrap();
    assert_eq!(
        serde_json::to_value(scanner.db.files().await.unwrap()).unwrap(),
        serde_json::to_value(&original).unwrap()
    );

    std::fs::write(scanner.root.join("edited.txt"), b"after edit").unwrap();
    std::fs::rename(
        scanner.root.join("moved.txt"),
        scanner.root.join("renamed.txt"),
    )
    .unwrap();
    std::fs::write(scanner.root.join("new.txt"), b"new file").unwrap();
    scanner.scan().await.unwrap();
    let current = scanner.db.file(&edited.id).await.unwrap();
    assert!(current.generation > edited.generation);
    assert_ne!(current.fingerprint, edited.fingerprint);
    assert_eq!(
        scanner.db.file(&moved.id).await.unwrap().relative_path,
        "renamed.txt"
    );
    assert_eq!(scanner.db.files().await.unwrap().len(), 3);

    std::fs::remove_file(scanner.root.join("edited.txt")).unwrap();
    scanner.scan().await.unwrap();
    let deleted = scanner.db.file(&edited.id).await.unwrap();
    assert!(deleted.materialized);
    assert!(deleted.fingerprint.is_none());
    assert_eq!(
        deleted.status, "tombstone",
        "never uploaded, so no cloud deletion is pending"
    );
    assert!(scanner
        .db
        .file(&moved.id)
        .await
        .unwrap()
        .fingerprint
        .is_some());
}
