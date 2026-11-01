//! Shared harness for the HTTP integration tests.
//!
//! Each test target compiles this module once and uses only part of it, so dead
//! code is expected.

#![allow(dead_code)]

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use cells::{
    app::State,
    auth::{Auth, Client},
    fs::{Export, Exports},
    router,
};
use std::{collections::BTreeMap, sync::Arc};

pub const SECRET: &str = "test-secret-at-least-24-bytes";
pub const ORIGIN: &str = "https://mindos.example";

/// A router over a single read-only `docs` export.
pub fn app(root: &std::path::Path) -> axum::Router {
    app_export(Export::open(root).unwrap())
}

/// A router over a single `docs` export with the given capability.
pub fn app_export(export: Export) -> axum::Router {
    router_for(state(
        export,
        None,
        vec!["docs".into()],
        vec!["docs".into()],
    ))
}

pub fn router_for(state: Arc<State>) -> axum::Router {
    router(state, &[ORIGIN.into()]).unwrap()
}

/// A state whose single export is reached through the `docs` alias.
pub fn state(
    export: Export,
    username: Option<String>,
    exports: Vec<String>,
    write_exports: Vec<String>,
) -> Arc<State> {
    let client = Client::new(SECRET.into(), username, exports, write_exports);
    State::with_secrets(
        Auth::new(Some("test-node".into()), vec![client]),
        Exports::new(BTreeMap::from([("docs".into(), Arc::new(export))])),
        [42; 32],
        "test-epoch".into(),
    )
}

pub fn request(uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {SECRET}"))
}

pub async fn body_json(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

pub async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

/// A JSON mutation request carrying an idempotency key.
pub fn mutate_request(id: &str, body: &str) -> axum::http::Request<Body> {
    request("/v1/fs/docs/mutate")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-operation-id", id)
        .body(Body::from(body.to_owned()))
        .unwrap()
}
