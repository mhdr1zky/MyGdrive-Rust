use mygdrive_sync::{
    db::Database,
    local::{self, Scanner},
};
use std::{ffi::OsStr, sync::Arc};
use tokio::sync::Mutex;

async fn scanner() -> (tempfile::TempDir, Scanner) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(&temp.path().join("state.sqlite3"), "test")
        .await
        .unwrap();
    (
        temp,
        Scanner {
            root,
            db,
            gate: Arc::new(Mutex::new(())),
        },
    )
}

#[test]
fn editor_sidecars_are_recognized_without_ignoring_arbitrary_user_files() {
    for name in [
        "~$report.docx",
        "~$BUDGET.XLSM",
        "~$slides.pptx",
        ".~lock.report.docx#",
        ".~lock.notes.odt#",
        "~WRD1234.tmp",
        "~WRL00AF.TMP",
        "~wrsabcd.tmp",
    ] {
        assert!(local::is_editor_temporary_name(OsStr::new(name)), "{name}");
    }
    for name in [
        "report.docx",
        "budget.xlsx",
        "notes.tmp",
        "Cedd4100",
        "~$budget.txt",
        "~$.docx",
        "~WRDreport.tmp",
        ".~lock.report.docx",
        "report.docx.bak",
        ".hidden-data",
    ] {
        assert!(!local::is_editor_temporary_name(OsStr::new(name)), "{name}");
    }
}

#[tokio::test]
async fn new_office_locks_and_scratch_files_never_enter_sync_metadata() {
    let (_temp, scanner) = scanner().await;
    for name in [
        "~$report.docx",
        "~$budget.xlsx",
        ".~lock.budget.xlsx#",
        "~WRD0001.tmp",
    ] {
        std::fs::write(scanner.root.join(name), b"editor-only working data").unwrap();
    }
    scanner.scan().await.unwrap();
    assert!(scanner.db.files().await.unwrap().is_empty());
    assert!(scanner.db.operations().await.unwrap().is_empty());
    assert_eq!(std::fs::read_dir(&scanner.root).unwrap().count(), 4);
}

#[tokio::test]
async fn office_atomic_save_promotes_the_temporary_bytes_to_the_original_identity() {
    let (_temp, scanner) = scanner().await;
    let document = scanner.root.join("report.docx");
    std::fs::write(&document, b"original document").unwrap();
    scanner.scan().await.unwrap();
    let original = scanner.db.files().await.unwrap().pop().unwrap();

    let working = scanner.root.join("~WRD1234.tmp");
    std::fs::write(&working, b"saved document contents").unwrap();
    std::fs::write(scanner.root.join("~$report.docx"), b"owner").unwrap();
    scanner.scan().await.unwrap();
    assert_eq!(scanner.db.files().await.unwrap().len(), 1);

    std::fs::rename(&working, &document).unwrap();
    scanner.scan().await.unwrap();
    let files = scanner.db.files().await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].id, original.id);
    assert_eq!(files[0].relative_path, "report.docx");
    assert_eq!(
        files[0].fingerprint.as_ref().unwrap().digest,
        local::fingerprint(&document).unwrap().digest
    );
    assert!(files[0].generation > original.generation);
}

#[tokio::test]
async fn anonymous_excel_save_files_and_general_tmp_data_remain_trackable() {
    let (_temp, scanner) = scanner().await;
    for name in ["Cedd4100", "notes.tmp"] {
        std::fs::write(scanner.root.join(name), b"potential real user data").unwrap();
    }
    scanner.scan().await.unwrap();
    let files = scanner.db.files().await.unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().any(|file| file.relative_path == "Cedd4100"));
    assert!(files.iter().any(|file| file.relative_path == "notes.tmp"));
}

#[tokio::test]
async fn existing_tracked_sidecars_are_preserved_when_the_filter_is_introduced() {
    let (_temp, scanner) = scanner().await;
    let path = scanner.root.join("~$legacy.docx");
    std::fs::write(&path, b"already tracked data").unwrap();
    scanner
        .db
        .record_local("~$legacy.docx", Some(local::fingerprint(&path).unwrap()))
        .await
        .unwrap();
    let original = scanner.db.files().await.unwrap().pop().unwrap();
    scanner.scan().await.unwrap();
    let tracked = scanner.db.file(&original.id).await.unwrap();
    assert_eq!(tracked.fingerprint, original.fingerprint);
    assert_eq!(std::fs::read(path).unwrap(), b"already tracked data");
}
