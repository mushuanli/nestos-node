#[path = "support/mod.rs"]
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use futures_util::stream;
use http_body_util::BodyExt;
use cells::{
    app::{Application, State},
    auth::{Auth, Client},
    fs::Exports,
    plugins::{EventFuture, EventStream, NodePlugin, PluginRegistry, PluginSettings},
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tower::ServiceExt;

struct Feature(tokio::sync::watch::Sender<u64>);
impl NodePlugin for Feature {
    fn id(&self) -> &'static str {
        "fixture"
    }
    fn subscribe<'a>(&'a self, _: &'a Arc<State>, identity: usize, args: Value) -> EventFuture<'a> {
        Box::pin(async move {
            if args["owner"] != identity {
                return Err(cells::core::error::Error::forbidden("EACCES"));
            }
            let changes = self.0.subscribe();
            Ok(Box::pin(stream::unfold(
                (changes, true),
                |(mut changes, first)| async move {
                    if !first && changes.changed().await.is_err() {
                        return None;
                    }
                    let revision = *changes.borrow_and_update();
                    Some((Ok(json!({"revision":revision})), (changes, false)))
                },
            )) as EventStream)
        })
    }
}
fn fixture() -> (Application, tokio::sync::watch::Sender<u64>) {
    let auth = Auth::new(
        Some("events".into()),
        vec![Client::new(common::SECRET.into(), None, vec![], vec![])],
    );
    let mut host = State::host(auth, Exports::new(BTreeMap::new())).unwrap();
    host.allowed_origins = vec![common::ORIGIN.into()];
    let changed = tokio::sync::watch::channel(0).0;
    let mut registry = PluginRegistry::empty();
    registry
        .register(Arc::new(Feature(changed.clone())))
        .unwrap();
    (
        Application::assemble(host, registry, &PluginSettings::default()).unwrap(),
        changed,
    )
}
fn request(subscriptions: Value) -> Request<Body> {
    common::request("/v1/events")
        .method("POST")
        .header("content-type", "application/json")
        .header("origin", common::ORIGIN)
        .body(Body::from(
            json!({"version":1,"subscriptions":subscriptions}).to_string(),
        ))
        .unwrap()
}
async fn update(body: &mut Body) -> Value {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Ok(bytes) = frame.into_data() {
            let text = std::str::from_utf8(&bytes).unwrap();
            if let Some(data) = text.lines().find_map(|line| line.strip_prefix("data: ")) {
                return serde_json::from_str(data).unwrap();
            }
        }
    }
}
#[tokio::test]
async fn multiplexes_plugin_sources_without_initializing_builtin_services_and_releases_admission() {
    let (app, changes) = fixture();
    let response = app.router().unwrap().oneshot(request(json!([
        {"id":"a","plugin":"fixture","args":{"owner":0}}, {"id":"b","plugin":"fixture","args":{"owner":0}}
    ]))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(app.state().event_slots.available_permits(), 31);
    let mut body = response.into_body();
    let a = update(&mut body).await;
    let b = update(&mut body).await;
    assert_ne!(a["id"], b["id"]);
    assert_eq!(a["data"]["revision"], 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(40), body.frame())
            .await
            .is_err()
    );
    changes.send_replace(1);
    assert_eq!(update(&mut body).await["data"]["revision"], 1);
    assert_eq!(update(&mut body).await["data"]["revision"], 1);
    drop(body);
    assert_eq!(app.state().event_slots.available_permits(), 32);
}
#[tokio::test]
async fn rejects_credentials_origins_duplicate_ids_disabled_plugins_and_cross_owner_sources() {
    let (app, _) = fixture();
    let router = app.router().unwrap();
    let valid = json!([{"id":"a","plugin":"fixture","args":{"owner":0}}]);
    let mut bad = request(valid.clone());
    bad.headers_mut().remove("authorization");
    assert_eq!(
        router.clone().oneshot(bad).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let mut bad = request(valid.clone());
    bad.headers_mut()
        .insert("origin", "https://untrusted.example".parse().unwrap());
    assert_eq!(
        router.clone().oneshot(bad).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    for (subscriptions, status) in [
        (json!([valid[0], valid[0]]), StatusCode::BAD_REQUEST),
        (json!([]), StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(
            router
                .clone()
                .oneshot(request(subscriptions))
                .await
                .unwrap()
                .status(),
            status
        );
    }
    assert_eq!(app.state().event_slots.available_permits(), 32);
}

#[tokio::test]
async fn a_rejected_source_does_not_close_another_authorized_subscription() {
    let (app, _) = fixture();
    let response = app
        .router()
        .unwrap()
        .oneshot(request(json!([
            {"id":"denied","plugin":"fixture","args":{"owner":1}},
            {"id":"allowed","plugin":"fixture","args":{"owner":0}}
        ])))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let a = update(&mut body).await;
    let b = update(&mut body).await;
    let frames = [a, b];
    assert!(frames
        .iter()
        .any(|row| row["id"] == "denied" && row["error"]["status"] == 403));
    assert!(frames
        .iter()
        .any(|row| row["id"] == "allowed" && row["data"]["revision"] == 0));
}

#[tokio::test]
async fn shutdown_closes_idle_streams_and_releases_connection_admission() {
    let (app, _) = fixture();
    let response = app
        .router()
        .unwrap()
        .oneshot(request(json!([
            {"id":"a","plugin":"fixture","args":{"owner":0}}
        ])))
        .await
        .unwrap();
    let mut body = response.into_body();
    update(&mut body).await;
    app.shutdown().await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(1), body.frame())
        .await
        .unwrap()
        .is_none());
    assert_eq!(app.state().event_slots.available_permits(), 32);
}
