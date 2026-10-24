use async_trait::async_trait;
use mygdrive_sync::{
    drive::{AccessTokenProvider, DriveClient, HttpDriveClient},
    model::RemoteFile,
    Error, Result, SyncEngine,
};
use sha2::{Digest as _, Sha256};
use std::sync::Arc;
use wiremock::{
    matchers::{header, method, path, query_param, query_param_is_missing},
    Mock, MockServer, ResponseTemplate,
};

struct TestToken;
#[async_trait]
impl AccessTokenProvider for TestToken {
    async fn access_token(&self) -> Result<String> {
        Ok("mock-token".into())
    }
}
fn metadata(id: &str, bytes: &[u8], version: &str) -> serde_json::Value {
    serde_json::json!({"id":id,"name":format!("{id}.txt"),"mimeType":"text/plain","version":version,
        "size":bytes.len().to_string(),"sha256Checksum":format!("{:x}",Sha256::digest(bytes)),"parents":["root"],"capabilities":{"canDownload":true}})
}
fn client(server: &MockServer) -> Arc<HttpDriveClient> {
    Arc::new(
        HttpDriveClient::with_endpoints(
            Arc::new(TestToken),
            &format!("{}/drive/v3/", server.uri()),
            &format!("{}/upload/drive/v3/", server.uri()),
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn paginated_inventory_and_feed_persist_cursor_and_download_real_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/drive/v3/files/root"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":"root","name":"root","mimeType":"application/vnd.google-apps.folder","version":"1"}))).mount(&server).await;
    Mock::given(path("/drive/v3/changes/startPageToken"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"startPageToken":"before-inventory"})),
        )
        .mount(&server)
        .await;
    let first = metadata("a", b"old", "1");
    let a = metadata("a", b"new server bytes", "2");
    let b = metadata("b", b"second file", "1");
    Mock::given(path("/drive/v3/files"))
        .and(query_param_is_missing("pageToken"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"files":[first],"nextPageToken":"inventory-2"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/drive/v3/files"))
        .and(query_param("pageToken", "inventory-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"files":[b]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/drive/v3/changes"))
        .and(query_param("pageToken", "before-inventory"))
        .and(query_param("pageSize", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"changes":[{"fileId":"a","file":a}],"nextPageToken":"changes-2"}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/drive/v3/changes"))
        .and(query_param("pageToken", "changes-2"))
        .and(query_param("pageSize", "1000"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"changes":[],"newStartPageToken":"caught-up"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    for (id, data, meta) in [
        ("a", b"new server bytes".as_slice(), a),
        ("b", b"second file".as_slice(), b),
    ] {
        Mock::given(path(format!("/drive/v3/files/{id}")))
            .and(query_param_is_missing("alt"))
            .and(header("authorization", "Bearer mock-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(meta))
            .mount(&server)
            .await;
        Mock::given(path(format!("/drive/v3/files/{id}")))
            .and(query_param("alt", "media"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(data))
            .expect(1)
            .mount(&server)
            .await;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sync");
    std::fs::create_dir(&root).unwrap();
    let engine = SyncEngine::open(
        &root,
        &temp.path().join("state"),
        "test",
        "root",
        client(&server),
    )
    .await
    .unwrap();
    engine
        .offline_select(&uuid::Uuid::new_v4().to_string(), "root", true)
        .await
        .unwrap();
    let report = engine.sync_once().await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        engine.db.cursor("changes").await.unwrap().as_deref(),
        Some("caught-up")
    );
    assert_eq!(
        std::fs::read(root.join("a.txt")).unwrap(),
        b"new server bytes"
    );
    assert_eq!(std::fs::read(root.join("b.txt")).unwrap(), b"second file");
    let complete: i64 = sqlx::query_scalar("SELECT count(*) FROM cache_objects WHERE complete=1")
        .fetch_one(&engine.db.pool)
        .await
        .unwrap();
    assert_eq!(complete, 2);
}

#[tokio::test]
async fn download_version_race_and_http_auth_failure_do_not_publish_cache() {
    let server = MockServer::start().await;
    let http = client(&server);
    let expected: RemoteFile = serde_json::from_value(metadata("a", b"content", "1")).unwrap();
    Mock::given(path("/drive/v3/files/a"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"content"))
        .mount(&server)
        .await;
    Mock::given(path("/drive/v3/files/a"))
        .and(query_param_is_missing("alt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata("a", b"new", "2")))
        .mount(&server)
        .await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sync");
    std::fs::create_dir(&root).unwrap();
    let engine = SyncEngine::open(
        &root,
        &temp.path().join("state"),
        "test",
        "root",
        http.clone(),
    )
    .await
    .unwrap();
    assert!(matches!(
        engine
            .cache
            .download_to_cache(http.as_ref(), &expected)
            .await,
        Err(Error::Changed)
    ));
    let complete: i64 = sqlx::query_scalar("SELECT count(*) FROM cache_objects")
        .fetch_one(&engine.db.pool)
        .await
        .unwrap();
    assert_eq!(complete, 0);
    Mock::given(path("/drive/v3/files/unauthorized"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    assert!(matches!(
        http.get("unauthorized").await,
        Err(Error::AuthRequired)
    ));
}

#[tokio::test]
async fn resumable_create_uses_preallocated_id_and_streams_content() {
    let server = MockServer::start().await;
    let http = client(&server);
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/reserved"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/upload/drive/v3/files"))
        .and(query_param("uploadType", "resumable"))
        .and(wiremock::matchers::body_partial_json(
            serde_json::json!({"id":"reserved","name":"file.txt","parents":["root"]}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("location", format!("{}/upload/session", server.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/upload/session"))
        .and(wiremock::matchers::body_bytes(b"original content"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata(
            "reserved",
            b"original content",
            "1",
        )))
        .expect(1)
        .mount(&server)
        .await;
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("snapshot");
    std::fs::write(&file, b"original content").unwrap();
    let snapshot = mygdrive_sync::model::Snapshot {
        path: file.clone(),
        digest: mygdrive_sync::local::fingerprint(&file).unwrap().digest,
    };
    let result = http
        .create("reserved", "file.txt", "root", &snapshot)
        .await
        .unwrap();
    assert_eq!(result.id, "reserved");
    assert_eq!(result.matches(&snapshot.digest), Some(true));
    assert!(http.conditional_mutations());
}

fn snapshot(temp: &tempfile::TempDir, bytes: &[u8]) -> mygdrive_sync::model::Snapshot {
    let file = temp.path().join(uuid::Uuid::new_v4().to_string());
    std::fs::write(&file, bytes).unwrap();
    mygdrive_sync::model::Snapshot {
        digest: mygdrive_sync::local::fingerprint(&file).unwrap().digest,
        path: file,
    }
}
fn v2(meta: &serde_json::Value, tag: Option<&str>) -> serde_json::Value {
    serde_json::json!({"id":meta["id"],"title":meta["name"],"mimeType":meta["mimeType"],
        "version":meta["version"],"fileSize":meta["size"],"md5Checksum":meta["md5Checksum"],
        "parents":meta["parents"].as_array().unwrap().iter().map(|p| serde_json::json!({"id":p})).collect::<Vec<_>>(),
        "labels":{"trashed":meta["trashed"].as_bool().unwrap_or(false)},"etag":tag})
}
fn with_md5(mut meta: serde_json::Value, bytes: &[u8]) -> serde_json::Value {
    use md5::Md5;
    meta["md5Checksum"] = format!("{:x}", Md5::digest(bytes)).into();
    meta
}

#[tokio::test]
async fn conditional_update_streams_actual_commit_with_strong_if_match() {
    let server = MockServer::start().await;
    let http = client(&server);
    let old = with_md5(metadata("a", b"old", "1"), b"old");
    Mock::given(method("GET"))
        .and(path("/drive/v2/files/a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(v2(&old, Some("\"revision-1\""))))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/upload/drive/v2/files/a"))
        .and(query_param("uploadType", "media"))
        .and(header("if-match", "\"revision-1\""))
        .and(wiremock::matchers::body_bytes(b"latest bytes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata("a", b"latest bytes", "2")))
        .mount(&server)
        .await;
    let temp = tempfile::tempdir().unwrap();
    let snap = snapshot(&temp, b"latest bytes");
    let updated = http
        .update(&serde_json::from_value(old).unwrap(), &snap)
        .await
        .unwrap();
    assert_eq!(updated.version, "2");
    assert_eq!(updated.matches(&snap.digest), Some(true));
}

#[tokio::test]
async fn conditional_mutations_fail_closed_without_etag_or_with_newer_version() {
    for (etag, version) in [
        (None, "1"),
        (Some("W/\"weak\""), "1"),
        (Some("\"revision-2\""), "2"),
    ] {
        let server = MockServer::start().await;
        let http = client(&server);
        let current = with_md5(metadata("a", b"server latest", version), b"server latest");
        Mock::given(method("GET"))
            .and(path("/drive/v2/files/a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(v2(&current, etag)))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let expected = serde_json::from_value(metadata("a", b"old", "1")).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let snap = snapshot(&temp, b"local edit");
        assert!(http.update(&expected, &snap).await.is_err());
        assert!(http.rename(&expected, "new.txt", "other").await.is_err());
        assert!(http.trash(&expected).await.is_err());
    }
}

#[tokio::test]
async fn server_race_returns_precondition_instead_of_overwriting() {
    let server = MockServer::start().await;
    let http = client(&server);
    let old = with_md5(metadata("a", b"old", "1"), b"old");
    Mock::given(method("GET"))
        .and(path("/drive/v2/files/a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(v2(&old, Some("\"revision-1\""))))
        .mount(&server)
        .await;
    Mock::given(header("if-match", "\"revision-1\""))
        .respond_with(ResponseTemplate::new(412))
        .expect(3)
        .mount(&server)
        .await;
    let expected = serde_json::from_value(old).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let snap = snapshot(&temp, b"local edit");
    assert!(matches!(
        http.update(&expected, &snap).await,
        Err(Error::Precondition)
    ));
    assert!(matches!(
        http.rename(&expected, "new.txt", "other").await,
        Err(Error::Precondition)
    ));
    assert!(matches!(
        http.trash(&expected).await,
        Err(Error::Precondition)
    ));
}

#[tokio::test]
async fn conditional_rename_moves_parents_and_delete_only_uses_trash() {
    for intent in ["rename", "trash"] {
        let server = MockServer::start().await;
        let http = client(&server);
        let old = with_md5(metadata("a", b"old", "1"), b"old");
        Mock::given(method("GET"))
            .and(path("/drive/v2/files/a"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(v2(&old, Some("\"revision-1\""))),
            )
            .mount(&server)
            .await;
        let mut updated = old.clone();
        let mut mutation = Mock::given(method("PATCH"))
            .and(path("/drive/v2/files/a"))
            .and(header("if-match", "\"revision-1\""));
        if intent == "rename" {
            updated["name"] = "renamed.txt".into();
            updated["parents"] = serde_json::json!(["other"]);
            mutation = mutation
                .and(query_param("addParents", "other"))
                .and(query_param("removeParents", "root"))
                .and(wiremock::matchers::body_json(
                    serde_json::json!({"title":"renamed.txt"}),
                ));
        } else {
            updated["trashed"] = true.into();
            mutation = mutation.and(wiremock::matchers::body_json(
                serde_json::json!({"labels":{"trashed":true}}),
            ));
        }
        mutation
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(updated))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let expected = serde_json::from_value(old).unwrap();
        if intent == "rename" {
            assert_eq!(
                http.rename(&expected, "renamed.txt", "other")
                    .await
                    .unwrap()
                    .parents,
                ["other"]
            );
        } else {
            assert!(http.trash(&expected).await.unwrap().trashed);
        }
    }
}

#[tokio::test]
async fn ambiguous_success_replays_acknowledge_intent_without_second_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let snap = snapshot(&temp, b"local edit");
    let expected: RemoteFile = serde_json::from_value(metadata("a", b"old", "1")).unwrap();
    for intent in ["update", "rename", "trash"] {
        let server = MockServer::start().await;
        let http = client(&server);
        let mut committed = with_md5(metadata("a", b"local edit", "2"), b"local edit");
        if intent == "rename" {
            committed["name"] = "renamed.txt".into();
            committed["parents"] = serde_json::json!(["other"]);
        }
        if intent == "trash" {
            committed["trashed"] = true.into();
        }
        Mock::given(method("GET"))
            .and(path("/drive/v2/files/a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(v2(&committed, None)))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        match intent {
            "update" => assert_eq!(http.update(&expected, &snap).await.unwrap().version, "2"),
            "rename" => assert_eq!(
                http.rename(&expected, "renamed.txt", "other")
                    .await
                    .unwrap()
                    .name,
                "renamed.txt"
            ),
            _ => assert!(http.trash(&expected).await.unwrap().trashed),
        }
    }
}

#[tokio::test]
async fn child_listing_is_paged_and_limits_query_to_requested_parent() {
    let server = MockServer::start().await;
    let http = client(&server);
    let query = "'par\\'ent' in parents and trashed = false";
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param("q", query))
        .and(query_param_is_missing("pageToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "files":[metadata("a",b"one","1")],"nextPageToken":"page2"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param("q", query))
        .and(query_param("pageToken", "page2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "files":[metadata("b",b"two","1")]})))
        .expect(1)
        .mount(&server)
        .await;
    let children = http.list_children("par'ent").await.unwrap();
    assert_eq!(
        children.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[tokio::test]
async fn ambiguous_create_replay_does_not_adopt_trashed_renamed_or_moved_identity() {
    let temp = tempfile::tempdir().unwrap();
    let snap = snapshot(&temp, b"original content");
    for divergent in ["trashed", "unavailable", "renamed", "moved", "same"] {
        let server = MockServer::start().await;
        let http = client(&server);
        let mut existing = metadata("reserved", b"original content", "2");
        existing["name"] = "file.txt".into();
        match divergent {
            "trashed" => existing["trashed"] = true.into(),
            "unavailable" => existing["unavailable"] = true.into(),
            "renamed" => existing["name"] = "other.txt".into(),
            "moved" => existing["parents"] = serde_json::json!(["other"]),
            _ => (),
        }
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/reserved"))
            .respond_with(ResponseTemplate::new(200).set_body_json(existing))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let result = http.create("reserved", "file.txt", "root", &snap).await;
        if divergent == "same" {
            assert_eq!(result.unwrap().version, "2");
        } else {
            assert!(matches!(result, Err(Error::Precondition)), "{divergent}");
        }
    }
}

#[tokio::test]
async fn empty_folder_trash_postcheck_restores_new_inherited_children() {
    use std::sync::atomic::{AtomicBool, Ordering};
    for scenario in [
        "empty",
        "explicit_only",
        "late_child",
        "replayed_late_child",
        "restore_failed",
    ] {
        let server = MockServer::start().await;
        let http = client(&server);
        let initially_trashed = scenario == "replayed_late_child";
        let trashed = Arc::new(AtomicBool::new(initially_trashed));
        let mut folder = metadata("folder", b"", "1");
        folder["mimeType"] = "application/vnd.google-apps.folder".into();
        folder["trashed"] = initially_trashed.into();
        if initially_trashed {
            folder["version"] = "2".into();
        }
        let expected: RemoteFile = serde_json::from_value(folder.clone()).unwrap();
        let folder_v2 = folder.clone();
        let state = trashed.clone();
        Mock::given(method("GET"))
            .and(path("/drive/v2/files/folder"))
            .respond_with(move |_: &wiremock::Request| {
                let is_trashed = state.load(Ordering::SeqCst);
                let mut meta = folder_v2.clone();
                meta["trashed"] = is_trashed.into();
                meta["version"] = if is_trashed { "2" } else { "1" }.into();
                ResponseTemplate::new(200).set_body_json(v2(
                    &meta,
                    Some(if is_trashed {
                        "\"trashed\""
                    } else {
                        "\"before\""
                    }),
                ))
            })
            .mount(&server)
            .await;
        let state = trashed.clone();
        let folder_v3 = folder.clone();
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/folder"))
            .respond_with(move |_: &wiremock::Request| {
                let mut meta = folder_v3.clone();
                meta["trashed"] = state.load(Ordering::SeqCst).into();
                ResponseTemplate::new(200).set_body_json(meta)
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .and(query_param("q", "'folder' in parents and trashed = false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"files":[]})))
            .expect(if initially_trashed { 0 } else { 1 })
            .mount(&server)
            .await;
        let children = match scenario {
            "empty" => serde_json::json!([]),
            "explicit_only" => serde_json::json!([{"id":"old-trash","explicitlyTrashed":true}]),
            _ => serde_json::json!([{"id":"late-file","explicitlyTrashed":false}]),
        };
        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .and(query_param("q", "'folder' in parents"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"files":children})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let state = trashed.clone();
        Mock::given(method("PATCH"))
            .and(path("/drive/v2/files/folder"))
            .and(header("if-match", "\"before\""))
            .and(wiremock::matchers::body_json(
                serde_json::json!({"labels":{"trashed":true}}),
            ))
            .respond_with(move |_: &wiremock::Request| {
                state.store(true, Ordering::SeqCst);
                ResponseTemplate::new(200)
            })
            .expect(if initially_trashed { 0 } else { 1 })
            .mount(&server)
            .await;
        let state = trashed.clone();
        let restore_fails = scenario == "restore_failed";
        Mock::given(method("PATCH"))
            .and(path("/drive/v2/files/folder"))
            .and(header("if-match", "\"trashed\""))
            .and(wiremock::matchers::body_json(
                serde_json::json!({"labels":{"trashed":false}}),
            ))
            .respond_with(move |_: &wiremock::Request| {
                if restore_fails {
                    ResponseTemplate::new(503)
                } else {
                    state.store(false, Ordering::SeqCst);
                    ResponseTemplate::new(200)
                }
            })
            .expect(if matches!(scenario, "empty" | "explicit_only") {
                0
            } else {
                1
            })
            .mount(&server)
            .await;
        let result = http.trash_empty_folder(&expected).await;
        match scenario {
            "empty" | "explicit_only" => {
                assert!(result.unwrap().trashed);
                assert!(trashed.load(Ordering::SeqCst));
            }
            "restore_failed" => assert!(matches!(result, Err(Error::Http { status: 503, .. }))),
            _ => {
                assert!(matches!(result, Err(Error::Precondition)));
                assert!(!trashed.load(Ordering::SeqCst));
            }
        }
    }
}
