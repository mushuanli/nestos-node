#[path = "support/mod.rs"]
mod common;
use axum::{body::Body, http::StatusCode, routing::get, Json, Router};
use cells::{
    app::{Application, State},
    config::Config,
    plugins::{NodePlugin, PluginRegistry},
};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

fn config(root: &std::path::Path) -> Config {
    toml::from_str(&format!(
        "listen='127.0.0.1:0'\nexecution=false\napi_key='{}'\n[[exports]]\nalias='docs'\npath='{}'",
        common::SECRET,
        root.display()
    ))
    .unwrap()
}

#[tokio::test]
async fn switches_remove_routes_and_discovery_without_opening_disabled_services() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config(root.path());
    config.execution = true;
    config.plugins = ["files", "process", "sync", "projects", "harness", "mcp"]
        .into_iter()
        .map(|id| (id.into(), false))
        .collect();
    config.sync = Some(cells::sync::Config {
        enabled: true,
        root: root.path().join("not-created"),
        ..Default::default()
    });
    config.projects = Some(cells::projects::Config {
        root: root.path().join("projects-not-created"),
        network: false,
    });
    config
        .harnesses
        .push(cells::harness::config::ProfileConfig {
            id: "disabled".into(),
            aliases: vec![],
            kind: "unknown".into(),
            command: "".into(),
            home: root.path().join("missing-home"),
            principal_id: None,
            projects: false,
            workspaces: vec![],
            dsh: None,
            codex: None,
        });
    let application = Application::from_config(&config).unwrap();
    application.start().await.unwrap();
    assert!(application
        .state()
        .services
        .get::<cells::sync::SyncService>()
        .is_none());
    assert!(!root.path().join("not-created").exists());
    assert!(!root.path().join("projects-not-created").exists());
    assert!(application
        .state()
        .services
        .get::<cells::harness::Harnesses>()
        .is_none());
    let router = application.router().unwrap();
    for route in [
        "/v1/exports",
        "/v1/processes",
        "/v1/sync/capabilities",
        "/mcp",
    ] {
        assert_eq!(
            router
                .clone()
                .oneshot(common::request(route).body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    let caps = common::body_json(
        router
            .oneshot(
                common::request("/v1/capabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(caps["files"]["read"], false);
    assert_eq!(caps["files"]["seq"]["sqlite"], false);
    assert_eq!(caps["process"]["exec"], false);
    assert_eq!(caps["harness"]["enabled"], false);
    assert_eq!(caps["plugins"]["sync"], false);
    application.shutdown().await.unwrap();
}

#[test]
fn rejects_unknown_plugins_and_missing_enabled_configuration() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config(root.path());
    config.plugins.insert("typo".into(), false);
    assert!(PluginRegistry::builtins()
        .resolve(&config)
        .err()
        .unwrap()
        .contains("Unknown node plugin"));
    config.plugins.clear();
    config.plugins.insert("sync".into(), true);
    assert!(PluginRegistry::builtins()
        .resolve(&config)
        .err()
        .unwrap()
        .contains("requires [sync]"));
}

struct Custom;
impl NodePlugin for Custom {
    fn id(&self) -> &'static str {
        "custom"
    }
    fn routes(&self) -> Router<Arc<State>> {
        Router::new().route("/v1/custom", get(|| async { Json(json!({"custom":true})) }))
    }
}

#[tokio::test]
async fn applications_accept_an_isolated_registry_and_protect_contributed_routes() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = PluginRegistry::empty();
    registry.register(Arc::new(Custom)).unwrap();
    let application = Application::with_registry(&config(root.path()), registry).unwrap();
    let router = application.router().unwrap();
    let request = axum::http::Request::builder()
        .uri("/v1/custom")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router.clone().oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let response = router
        .clone()
        .oneshot(common::request("/v1/custom").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(common::body_json(response).await, json!({"custom":true}));
    assert_eq!(
        router
            .oneshot(common::request("/v1/exports").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[test]
fn cli_checks_one_plugin_without_starting_or_leaking_configuration() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    std::fs::write(&path, format!("listen='127.0.0.1:0'\napi_key='{}'\n[[harnesses]]\nid='broken'\nkind='unknown'\nhome='/missing-test-home'", common::SECRET)).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cells"))
        .args(["plugins", "check"])
        .arg(&path)
        .arg("files")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["order"], json!(["files"]));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(common::SECRET));
    assert!(!root.path().join("logs").exists());
}

#[tokio::test]
async fn disabled_project_claims_cannot_fall_back_to_unscoped_file_access() {
    let root = tempfile::tempdir().unwrap();
    let application = Application::from_config(&config(root.path())).unwrap();
    let request = common::request("/v1/fs/docs/stat")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-fsagent-project", "disabled-scope")
        .body(Body::from("{\"paths\":[\"\"]}"))
        .unwrap();
    let response = application
        .router()
        .unwrap()
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(common::body_json(response)
        .await
        .to_string()
        .contains("ECAPABILITY"));
    application.shutdown().await.unwrap();
}
