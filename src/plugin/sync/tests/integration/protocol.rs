use super::support::*;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use cells::router;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn three_devices_publish_cas_receipts_and_range_downloads() {
    let root = tempfile::tempdir().unwrap();
    let storage = service(root.path());
    project(&storage);
    let first = manifest(&storage, "first");
    let head = dataset(&storage, &first);
    activate(&storage, "B");
    activate(&storage, "C");
    drop(storage);
    let state = state(root.path());
    let service = state
        .services
        .get::<cells::sync::SyncService>()
        .unwrap()
        .clone();
    let epoch = service.epoch().to_owned();
    let app = router(state.clone(), &[]).unwrap();
    let bytes = b"second".to_vec();
    let content = put(&app, &epoch, "p", bytes.clone()).await;
    let next = put(
        &app,
        &epoch,
        "p",
        serde_json::to_vec(&json!({"format":"fs-agent.files","version":1,
        "entries":[{"path":"a.txt","kind":"file","hash":content,"size":"6"}]}))
        .unwrap(),
    )
    .await;
    let request = command(
        &service,
        "A",
        3,
        json!({"expectedHead":head,"nextManifestHash":next}),
    );
    let (status, result) = call(
        &app,
        &epoch,
        "POST",
        "projects/p/datasets/files/publish",
        request.clone(),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(result["result"]["head"]["generation"], "2");
    assert_eq!(
        call(
            &app,
            &epoch,
            "POST",
            "projects/p/datasets/files/publish",
            request.clone()
        )
        .await
        .1,
        result
    );
    let b = command(
        &service,
        "B",
        1,
        json!({"expectedHead":head,"nextManifestHash":next}),
    );
    let (status, conflict) =
        call(&app, &epoch, "POST", "projects/p/datasets/files/publish", b).await;
    assert_eq!(status, 412);
    assert_eq!(conflict["code"], "HEAD_CONFLICT");
    let mut reused = request.clone();
    reused["nextManifestHash"] = json!(first);
    assert_eq!(
        call(
            &app,
            &epoch,
            "POST",
            "projects/p/datasets/files/publish",
            reused
        )
        .await
        .1["code"],
        "OPERATION_REUSED"
    );
    let (_, changes) = call(&app, &epoch, "GET", "projects/p/changes", json!({})).await;
    assert_eq!(changes["changes"].as_array().unwrap().len(), 2);
    let req = Request::builder()
        .uri(format!("/v1/sync/projects/p/objects/{content}"))
        .header("authorization", "Bearer sync-test-secret-at-least-24-bytes")
        .header("x-sync-history-epoch", &epoch)
        .header("range", "bytes=1-3")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), 206);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"eco"
    );
    assert_eq!(service.operation("A", "3").unwrap(), result);
    assert_eq!(
        service.operation("B", "1").unwrap()["outcome"],
        "not-committed"
    );
}
#[test]
fn fixed_catalog_pages_and_changes_discover_new_sessions() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let hash = manifest(&s, "x");
    dataset(&s, &hash);
    let cat = s.catalog("p", None, "all", 1).unwrap();
    let round = install(&s, "p", b"{}");
    let bundle=install(&s,"p",&serde_json::to_vec(&json!({"format":"fs-agent.bundle","version":1,
        "mediaType":"application/vnd.itookit.session+json","root":round,"objects":[{"hash":round,"size":"2"}]})).unwrap());
    let result=s.command("projects/p/datasets",&command(&s,"A",3,json!({"datasetId":"s1","logicalId":"logical-1","kind":"session","manifestHash":bundle})),false).unwrap();
    assert_eq!(result["outcome"], "committed");
    let next = s.catalog("p", cat["cursor"].as_str(), "all", 1).unwrap();
    assert!(next["datasets"].as_array().unwrap().is_empty());
    let change = s.changes("p", cat["changesCursor"].as_str(), 1).unwrap();
    assert_eq!(change["changes"][0]["dataset"]["logicalId"], "logical-1");
    let mut cursor = cat["cursor"].as_str().unwrap().to_owned();
    cursor.push('a');
    assert_eq!(
        s.catalog("p", Some(&cursor), "all", 1).unwrap_err().code,
        "CURSOR_EXPIRED"
    );
}
#[test]
fn manifest_closure_namespace_and_sequence_checks() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let fake = install(
        &s,
        "p",
        &serde_json::to_vec(&json!({"format":"fs-agent.files","version":1,
        "entries":[{"path":"a","kind":"file","hash":"0".repeat(64),"size":"1"}]}))
        .unwrap(),
    );
    let result = s
        .command(
            "projects/p/datasets",
            &command(
                &s,
                "A",
                2,
                json!({"datasetId":"files","logicalId":"files","kind":"files","manifestHash":fake}),
            ),
            false,
        )
        .unwrap();
    assert_eq!(result["code"], "OBJECT_MISSING");
    let c = command(&s, "A", 4, json!({"projectId":"later"}));
    assert_eq!(
        s.command("projects", &c, false).unwrap_err().code,
        "OPERATION_SEQUENCE"
    );
    assert_eq!(
        s.reserve("../p", &"0".repeat(64), 1).unwrap_err().code,
        "INVALID_ID"
    );
    s.register(&json!({"replicaId":"reconciling"})).unwrap();
    assert_eq!(
        s.command(
            "projects",
            &command(&s, "reconciling", 1, json!({"projectId":"later"})),
            false
        )
        .unwrap_err()
        .code,
        "REPLICA_EXPIRED"
    );
}

#[test]
fn shared_cross_language_fixtures_have_fixed_hashes() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/manifests.json")).unwrap();
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    for fixture in fixtures.as_array().unwrap() {
        let bytes = fixture["canonical"].as_str().unwrap().as_bytes();
        assert_eq!(hash(bytes), fixture["sha256"]);
        let h = install(&s, "p", bytes);
        let seq = s.replica("A").unwrap()["lastAdmittedSeq"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            + 1;
        let name = fixture["name"].as_str().unwrap();
        let r = s
            .command(
                "projects/p/datasets",
                &command(
                    &s,
                    "A",
                    seq,
                    json!({"datasetId":name,"logicalId":name,"kind":"files","manifestHash":h}),
                ),
                false,
            )
            .unwrap();
        assert_eq!(r["outcome"], "committed");
    }
}

#[test]
fn concurrent_writers_commit_exactly_one_new_generation() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let first = manifest(&s, "first");
    let head = dataset(&s, &first);
    let next = manifest(&s, "next");
    activate(&s, "B");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = [("A", 3), ("B", 1)]
        .into_iter()
        .map(|(replica, seq)| {
            let s = s.clone();
            let barrier = barrier.clone();
            let head = head.clone();
            let next = next.clone();
            std::thread::spawn(move || {
                barrier.wait();
                s.command(
                    "projects/p/datasets/files/publish",
                    &command(
                        &s,
                        replica,
                        seq,
                        json!({"expectedHead":head,"nextManifestHash":next}),
                    ),
                    false,
                )
                .unwrap()
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        results
            .iter()
            .filter(|r| r["outcome"] == "committed")
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| r["code"] == "HEAD_CONFLICT")
            .count(),
        1
    );
    assert_eq!(s.head("p", "files").unwrap()["generation"], "2");
}
#[tokio::test]
async fn object_and_manifest_stream_limits_are_independent_of_json_commands() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    drop(s);
    let state = state(root.path());
    let service = state
        .services
        .get::<cells::sync::SyncService>()
        .unwrap()
        .clone();
    let epoch = service.epoch().to_owned();
    let app = router(state, &[]).unwrap();
    let content = put(&app, &epoch, "p", vec![b'x'; 600000]).await;
    let entries: Vec<_> = (0..6000)
        .map(|n| json!({"path":format!("f{n:05}"),"kind":"file","hash":content,"size":"600000"}))
        .collect();
    let bytes =
        serde_json::to_vec(&json!({"format":"fs-agent.files","version":1,"entries":entries}))
            .unwrap();
    assert!(bytes.len() > 512 * 1024);
    let manifest = put(&app, &epoch, "p", bytes).await;
    let request = command(
        &service,
        "A",
        2,
        json!({"datasetId":"files","logicalId":"files","kind":"files","manifestHash":manifest}),
    );
    assert_eq!(
        call(&app, &epoch, "POST", "projects/p/datasets", request)
            .await
            .0,
        200
    );
    let (status, value) = call(
        &app,
        "old-epoch",
        "GET",
        "replicas/A/operations/1",
        json!({}),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(value["code"], "HISTORY_EPOCH_CHANGED");
}
