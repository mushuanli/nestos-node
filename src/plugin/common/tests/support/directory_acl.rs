use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use cells::{app::State, config::Config};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};
use tower::ServiceExt;
pub const ALICE: &str = "alice-directory-token-0123456789";
pub const BOB: &str = "bob-directory-token-01234567890";

pub fn config(root: &Path, reversed: bool) -> Config {
    let alice = format!("[[principals]]\nid='alice'\ntoken='{ALICE}'\n");
    let bob = format!("[[principals]]\nid='bob'\ntoken='{BOB}'\n");
    let principals = if reversed { bob + &alice } else { alice + &bob };
    toml::from_str(&format!("listen='127.0.0.1:0'\nexecution=false\nserver_id='directory-acl-test'\n{principals}\n[projects]\nroot={:?}\n[[exports]]\nalias='docs'\npath={:?}\naccess='rw'\npublic=true\n[[exports.grants]]\nprincipal='alice'\naccess='rw'\n[[exports.grants]]\nprincipal='bob'\naccess='ro'\n[[exports]]\nalias='private'\npath={:?}\naccess='rw'\n[[exports.grants]]\nprincipal='alice'\naccess='rw'\n",root.join("catalog"),root.join("docs"),root.join("private"))).unwrap()
}
pub fn fixture() -> (tempfile::TempDir, Arc<State>) {
    fixture_at(tempfile::tempdir().unwrap())
}
pub fn fixture_at(root: tempfile::TempDir) -> (tempfile::TempDir, Arc<State>) {
    for name in ["docs", "private"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    std::fs::write(root.path().join("docs/readme.txt"), "public fixture").unwrap();
    std::fs::write(root.path().join("private/secret.txt"), "private fixture").unwrap();
    let state = State::from_config(&config(root.path(), false)).unwrap();
    (root, state)
}
pub async fn request(
    state: &Arc<State>,
    token: Option<&str>,
    method: &str,
    uri: &str,
    value: Value,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if method == "PUT" {
        request = request
            .header("if-none-match", "*")
            .header("x-operation-id", "acl-denied-put");
    }
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = cells::router(state.clone(), &[])
        .unwrap()
        .oneshot(request.body(Body::from(value.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}
pub async fn exports(state: &Arc<State>, token: Option<&str>) -> Value {
    let (status, bytes) = request(state, token, "GET", "/v1/exports", json!(null)).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice(&bytes).unwrap()
}
