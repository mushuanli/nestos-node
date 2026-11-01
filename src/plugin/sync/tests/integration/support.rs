use axum::{body::Body, http::Request, Router};
use http_body_util::BodyExt;
use cells::{
    app::State,
    sync::{Config, SyncService},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};
use tower::ServiceExt;

pub fn config(root: &Path) -> Config {
    Config {
        enabled: true,
        root: root.to_owned(),
        metadata_reserve_bytes: 0,
        ..Config::default()
    }
}
pub fn service(root: &Path) -> Arc<SyncService> {
    let c = config(root);
    SyncService::init(&c).unwrap();
    SyncService::open(&c).unwrap()
}
pub fn state(root: &Path) -> Arc<State> {
    let body=format!("listen='127.0.0.1:0'\nexecution=false\ntoken='sync-test-secret-at-least-24-bytes'\n[sync]\nenabled=true\nroot='{}'\nmetadata_reserve_bytes=0",root.display());
    State::from_config(&toml::from_str(&body).unwrap()).unwrap()
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub async fn call(
    app: &Router,
    epoch: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (u16, Value) {
    let bytes = if method == "GET" {
        Vec::new()
    } else {
        serde_json::to_vec(&body).unwrap()
    };
    let request = Request::builder()
        .method(method)
        .uri(format!("/v1/sync/{path}"))
        .header("authorization", "Bearer sync-test-secret-at-least-24-bytes")
        .header("x-sync-history-epoch", epoch)
        .header("content-type", "application/json")
        .body(Body::from(bytes))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
pub async fn put(app: &Router, epoch: &str, project: &str, bytes: Vec<u8>) -> String {
    let h = hash(&bytes);
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/v1/sync/projects/{project}/objects/{h}"))
        .header("authorization", "Bearer sync-test-secret-at-least-24-bytes")
        .header("x-sync-history-epoch", epoch)
        .header("content-length", bytes.len())
        .body(Body::from(bytes))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert!(
        response.status().is_success(),
        "{}",
        String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes())
    );
    h
}
pub fn command(service: &SyncService, replica: &str, seq: u64, extra: Value) -> Value {
    let caps = service.capabilities();
    let mut body = json!({"authorityId":caps["authorityId"],"historyEpoch":caps["historyEpoch"],"replicaId":replica,
        "operationId":format!("{replica}-{seq}"),"opSeq":seq.to_string(),"expectedProjectLifecycleRevision":"1"});
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    body
}
pub fn activate(service: &SyncService, replica: &str) {
    service.register(&json!({"replicaId":replica})).unwrap();
    let projects = service.projects("all").unwrap();
    let scopes: Vec<_> = projects["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let id = p["projectId"].as_str().unwrap();
            let cat = service.catalog(id, None, "all", 1000).unwrap();
            json!({"projectId":id,"cursor":cat["cursor"]})
        })
        .collect();
    service
        .activate(replica, &json!({"scopes":scopes}))
        .unwrap();
}
pub fn project(service: &SyncService) {
    activate(service, "A");
    let result = service
        .command(
            "projects",
            &command(service, "A", 1, json!({"projectId":"p"})),
            false,
        )
        .unwrap();
    assert_eq!(result["outcome"], "committed");
}
pub fn install(service: &SyncService, project: &str, bytes: &[u8]) -> String {
    let h = hash(bytes);
    let id = service.reserve(project, &h, bytes.len() as u64).unwrap();
    let file = service.config.root.join("staging").join(&id);
    std::fs::write(&file, bytes).unwrap();
    std::fs::File::open(file).unwrap().sync_all().unwrap();
    service.install(&id).unwrap();
    h
}
pub fn manifest(service: &SyncService, content: &str) -> String {
    let h = install(service, "p", content.as_bytes());
    let value = json!({"format":"fs-agent.files","version":1,"entries":[{"path":"a.txt","kind":"file","hash":h,"size":content.len().to_string()}]});
    install(service, "p", &serde_json::to_vec(&value).unwrap())
}
pub fn dataset(service: &SyncService, hash: &str) -> Value {
    let result = service
        .command(
            "projects/p/datasets",
            &command(
                service,
                "A",
                2,
                json!({"datasetId":"files","kind":"files","logicalId":"files","manifestHash":hash}),
            ),
            false,
        )
        .unwrap();
    assert_eq!(result["outcome"], "committed", "{result}");
    result["result"]["head"].clone()
}
