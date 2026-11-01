use super::support::*;
use cells::sync::SyncService;
use serde_json::json;

#[test]
fn delete_restore_history_and_lifecycle_fence() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let hash = manifest(&s, "old");
    let head = dataset(&s, &hash);
    let next = manifest(&s, "new");
    s.command(
        "projects/p/datasets/files/publish",
        &command(
            &s,
            "A",
            3,
            json!({"expectedHead":head,"nextManifestHash":next}),
        ),
        false,
    )
    .unwrap();
    let versions = s.versions("p", "files", None, None, 100).unwrap();
    assert_eq!(versions["versions"][0]["contentStatus"], "available");
    assert!(
        versions["versions"][0]["retainUntil"].as_u64().unwrap()
            > versions["versions"][0]["committedAt"].as_u64().unwrap()
    );
    let current = s.head("p", "files").unwrap();
    let current_head =
        json!({"generation":current["generation"],"manifestHash":current["manifestHash"]});
    s.command(
        "projects/p/datasets/files/publish",
        &command(
            &s,
            "A",
            4,
            json!({"expectedHead":current_head,"nextManifestHash":hash}),
        ),
        false,
    )
    .unwrap();
    assert_eq!(s.head("p", "files").unwrap()["generation"], "3");
    let old_request = command(
        &s,
        "A",
        7,
        json!({"expectedHead":{"generation":"3","manifestHash":hash},"nextManifestHash":next}),
    );
    s.command("projects/p/delete", &command(&s, "A", 5, json!({})), false)
        .unwrap();
    assert_eq!(
        s.projects("deleted").unwrap()["projects"][0]["projectId"],
        "p"
    );
    let restore = s
        .command(
            "projects/p/restore",
            &command(&s, "A", 6, json!({"expectedProjectLifecycleRevision":"2"})),
            false,
        )
        .unwrap();
    assert_eq!(restore["outcome"], "committed");
    assert_eq!(
        s.command("projects/p/datasets/files/publish", &old_request, false)
            .unwrap()["code"],
        "PROJECT_LIFECYCLE_CHANGED"
    );
    let delete=s.command("projects/p/datasets/files/delete",&command(&s,"A",8,json!({"expectedProjectLifecycleRevision":"3","expectedHead":{"generation":"3","manifestHash":hash}})),false).unwrap();
    assert_eq!(delete["outcome"], "committed");
    let restore=s.command("projects/p/datasets/files/restore",&command(&s,"A",9,json!({"expectedProjectLifecycleRevision":"3","expectedDeletedGeneration":"4","sourceGeneration":"1"})),false).unwrap();
    assert_eq!(restore["outcome"], "committed");
    assert_eq!(s.head("p", "files").unwrap()["generation"], "5");
}
#[test]
fn backup_empty_restore_epoch_rejoin_and_damage_repair() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    let c = config(&root);
    SyncService::init(&c).unwrap();
    let s = SyncService::open(&c).unwrap();
    project(&s);
    let h = manifest(&s, "old");
    dataset(&s, &h);
    let backup = parent.path().join("backup");
    s.backup(&backup).unwrap();
    let old_epoch = s.epoch().to_owned();
    drop(s);
    let new_root = parent.path().join("restored");
    let restored_c = config(&new_root);
    let config_path = parent.path().join("restore.toml");
    std::fs::write(&config_path,format!("listen='127.0.0.1:0'\nexecution=false\ntoken='sync-test-secret-at-least-24-bytes'\n[sync]\nenabled=true\nroot='{}'\nmetadata_reserve_bytes=0",new_root.display())).unwrap();
    cells::sync::admin(&[
        "restore".into(),
        config_path.to_string_lossy().into_owned(),
        backup.to_string_lossy().into_owned(),
    ])
    .unwrap();
    let restored = SyncService::open(&restored_c).unwrap();
    assert_ne!(restored.epoch(), old_epoch);
    assert_eq!(restored.head("p", "files").unwrap()["manifestHash"], h);
    let mut old = command(&restored, "A", 3, json!({"projectId":"q"}));
    old["historyEpoch"] = json!(old_epoch);
    assert_eq!(
        restored.command("projects", &old, false).unwrap_err().code,
        "HISTORY_EPOCH_CHANGED"
    );
    activate(&restored, "fresh");
    assert_eq!(
        restored
            .command(
                "projects",
                &command(&restored, "fresh", 1, json!({"projectId":"q"})),
                false
            )
            .unwrap()["outcome"],
        "committed"
    );
    let content = hash(b"old");
    let path = new_root
        .join("objects/personal/p")
        .join(&content[..2])
        .join(&content);
    std::fs::write(&path, b"bad").unwrap();
    assert_eq!(
        restored.object_file("p", &content).err().unwrap().code,
        "OBJECT_CORRUPT"
    );
    assert_eq!(
        restored
            .versions("p", "files", Some("1"), None, 100)
            .unwrap()["contentStatus"],
        "corrupt"
    );
    let source = parent.path().join("source");
    std::fs::write(&source, b"old").unwrap();
    restored.repair("p", &content, &source).unwrap();
    restored.verify().unwrap();
    assert_eq!(restored.head("p", "files").unwrap()["generation"], "1");
}
#[test]
fn missing_database_and_storage_lock_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    assert!(SyncService::open(&config(root.path())).is_err());
    drop(s);
    std::fs::remove_file(root.path().join("metadata.db")).unwrap();
    assert!(SyncService::open(&config(root.path())).is_err());
    assert!(!root.path().join("metadata.db").exists());
    assert!(SyncService::init(&config(root.path())).is_err());
}

#[test]
fn old_current_versions_get_a_new_retention_window_and_quota_protects_them() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let first = manifest(&s, "old");
    let head = dataset(&s, &first);
    let db = rusqlite::Connection::open(root.path().join("metadata.db")).unwrap();
    let key = "files/00000000000000000001";
    let raw: String = db
        .query_row(
            "SELECT value FROM records WHERE scope='p' AND kind='version' AND key=?1",
            [key],
            |r| r.get(0),
        )
        .unwrap();
    let mut version: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    version["committedAt"] = json!(now - 90 * 86400);
    db.execute(
        "UPDATE records SET value=?1 WHERE scope='p' AND kind='version' AND key=?2",
        (version.to_string(), key),
    )
    .unwrap();
    let next = manifest(&s, "new");
    s.command(
        "projects/p/datasets/files/publish",
        &command(
            &s,
            "A",
            3,
            json!({"expectedHead":head,"nextManifestHash":next}),
        ),
        false,
    )
    .unwrap();
    let old = s.versions("p", "files", Some("1"), None, 100).unwrap();
    assert!(old["retainUntil"].as_u64().unwrap() >= now + 30 * 86400);
    assert_eq!(old["contentStatus"], "available");
    let used: u64 = db
        .query_row("SELECT SUM(size) FROM objects", [], |r| r.get(0))
        .unwrap();
    drop(db);
    drop(s);
    let mut c = config(root.path());
    c.max_object_bytes = 1;
    c.max_manifest_bytes = 1;
    c.max_retained_bytes = used;
    let s = SyncService::open(&c).unwrap();
    assert_eq!(
        s.reserve("p", &hash(b"q"), 1).unwrap_err().code,
        "QUOTA_EXCEEDED"
    );
    s.gc().unwrap();
    assert!(s.object_file("p", &hash(b"old")).is_ok());
}
#[test]
fn pins_protect_expired_versions_and_gc_reclaims_after_the_window() {
    let root = tempfile::tempdir().unwrap();
    let mut c = config(root.path());
    c.history_retention_seconds = 1;
    c.operation_retention_seconds = 1;
    c.upload_ttl_seconds = 1;
    c.read_pin_seconds = 4;
    SyncService::init(&c).unwrap();
    let s = SyncService::open(&c).unwrap();
    project(&s);
    let old = manifest(&s, "old");
    let head = dataset(&s, &old);
    let next = manifest(&s, "new");
    s.command(
        "projects/p/datasets/files/publish",
        &command(
            &s,
            "A",
            3,
            json!({"expectedHead":head,"nextManifestHash":next}),
        ),
        false,
    )
    .unwrap();
    s.pin(
        "p",
        &json!({"manifestHash":old,"requestKey":"reading","ttlSeconds":4}),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    s.gc().unwrap();
    assert!(s.object_file("p", &hash(b"old")).is_ok());
    assert_eq!(s.operation("A", "3").unwrap_err().code, "OPERATION_EXPIRED");
    let retry = command(
        &s,
        "A",
        3,
        json!({"expectedHead":head,"nextManifestHash":next}),
    );
    assert_eq!(
        s.command("projects/p/datasets/files/publish", &retry, false)
            .unwrap_err()
            .code,
        "OPERATION_EXPIRED"
    );
    let create = command(&s, "A", 4, json!({"projectId":"q"}));
    assert_eq!(
        s.command("projects", &create, false).unwrap()["outcome"],
        "committed"
    );
    std::thread::sleep(std::time::Duration::from_secs(2));
    s.gc().unwrap();
    assert!(s.object_file("p", &hash(b"old")).is_err());
    assert!(s.object_file("p", &hash(b"new")).is_ok());
}

#[tokio::test]
async fn sandbox_cannot_access_sync_storage_via_paths_or_descriptors() {
    if std::env::var("CELLS_PROCESS_TEST").as_deref() != Ok("1") {
        return;
    }
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("sync");
    let work = parent.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let c = config(&root);
    SyncService::init(&c).unwrap();
    let app_config:cells::config::Config=toml::from_str(&format!("listen='127.0.0.1:0'\nexecution=true\nserver_id='test-node'\ntoken='sync-test-secret-at-least-24-bytes'\n[sync]\nenabled=true\nroot='{}'\nmetadata_reserve_bytes=0\n[[exports]]\npath='{}'\nalias='work'\naccess='rw'",root.display(),work.display())).unwrap();
    let state = cells::app::State::from_config(&app_config).unwrap();
    cells::process::enable(&state).await.unwrap();
    let app = cells::router(state.clone(), &[]).unwrap();
    let script=format!("set -eu; test ! -e '{}'; test ! -e /workspace/../sync; for fd in /proc/self/fd/*; do target=$(readlink \"$fd\" || true); case \"$target\" in *metadata.db*|*sync.lock*) exit 8;; esac; done; printf isolated",root.display());
    let body = json!({"serverId":"test-node","epoch":state.services.require::<cells::process::Execution>().unwrap().epoch(),"requestId":"sync-isolation","command":"/bin/bash","args":["-c",script],"cwd":"/workspace","timeoutMs":5000,"mounts":[{"alias":"work","path":"","at":"/workspace","access":"rw"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/processes")
        .header("authorization", "Bearer sync-test-secret-at-least-24-bytes")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert!(response.status().is_success());
    for _ in 0..500 {
        let req = Request::builder()
            .uri(format!(
                "/v1/processes/{}/sync-isolation",
                state
                    .services
                    .require::<cells::process::Execution>()
                    .unwrap()
                    .epoch()
            ))
            .header("authorization", "Bearer sync-test-secret-at-least-24-bytes")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert!(response.status().is_success());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        if result["state"] != "running" {
            assert_eq!(result["code"], 0, "{result}");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("sandbox did not terminate");
}

#[test]
fn cancellation_partial_project_recovery_and_expired_replica_are_explicit() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let h = manifest(&s, "old");
    dataset(&s, &h);
    let canceled = command(
        &s,
        "A",
        3,
        json!({"expectedHead":{"generation":"1","manifestHash":h},"nextManifestHash":h}),
    );
    let result = s
        .cancel_operation(
            "A",
            "3",
            &json!({"target":"projects/p/datasets/files/publish","command":canceled}),
        )
        .unwrap();
    assert_eq!(result["code"], "CANCELLED");
    assert_eq!(
        s.command("projects/p/datasets/files/publish", &canceled, false)
            .unwrap(),
        result
    );
    s.command("projects/p/delete", &command(&s, "A", 4, json!({})), false)
        .unwrap();
    let content = hash(b"old");
    let path = root
        .path()
        .join("objects/personal/p")
        .join(&content[..2])
        .join(&content);
    std::fs::remove_file(path).unwrap();
    assert!(s.object_file("p", &content).is_err());
    let failed = s
        .command(
            "projects/p/restore",
            &command(&s, "A", 5, json!({"expectedProjectLifecycleRevision":"2"})),
            false,
        )
        .unwrap();
    assert_eq!(failed["outcome"], "not-committed");
    assert_eq!(
        s.projects("deleted").unwrap()["projects"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let db = rusqlite::Connection::open(root.path().join("metadata.db")).unwrap();
    db.execute("UPDATE records SET value=json_set(value,'$.lastSeen',0) WHERE scope=?1 AND kind='replica' AND key='A'",[s.epoch()]).unwrap();
    assert_eq!(s.replica("A").unwrap()["state"], "expired");
    assert_eq!(
        s.command(
            "projects",
            &command(&s, "A", 6, json!({"projectId":"q"})),
            false
        )
        .unwrap_err()
        .code,
        "REPLICA_EXPIRED"
    );
    activate(&s, "replacement");
    assert_eq!(
        s.command(
            "projects",
            &command(&s, "replacement", 1, json!({"projectId":"q"})),
            false
        )
        .unwrap()["outcome"],
        "committed"
    );
}
#[test]
fn export_overlap_identity_change_and_oversized_upload_are_rejected() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("sync");
    let c = config(&root);
    SyncService::init(&c).unwrap();
    let config:cells::config::Config=toml::from_str(&format!("listen='127.0.0.1:0'\nexecution=false\ntoken='sync-test-secret-at-least-24-bytes'\n[sync]\nenabled=true\nroot='{}'\n[[exports]]\npath='{}'\nalias='all'",root.display(),parent.path().display())).unwrap();
    assert!(cells::app::State::from_config(&config)
        .err()
        .unwrap()
        .contains("SYNC_EXPORT_OVERLAP"));
    let mut wrong = c.clone();
    wrong.namespace_id = "other".into();
    assert!(SyncService::open(&wrong).is_err());
    let s = SyncService::open(&c).unwrap();
    project(&s);
    assert_eq!(
        s.reserve("p", &hash(b"q"), s.config.max_object_bytes + 1)
            .unwrap_err()
            .code,
        "LIMIT_EXCEEDED"
    );
    let h = install(&s, "p", b"q");
    assert!(s.reuse("p", &h, 1).unwrap().is_some());
    assert_eq!(
        s.reuse("p", &h, 2).unwrap_err().code,
        "OBJECT_SIZE_MISMATCH"
    );
}
