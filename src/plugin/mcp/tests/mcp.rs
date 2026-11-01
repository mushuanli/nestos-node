//! MCP contracts, directory discovery and per-request authorization.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use http_body_util::BodyExt;
use cells::{app::State, projects};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
#[allow(dead_code)]
#[path = "../../common/tests/support/directory_acl.rs"]
mod support;
use support::{ALICE, BOB};

fn request(token: Option<&str>, method: &str, mut params: Value) -> Request<Body> {
    params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}});
    let mut builder = Request::post("/mcp")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-method", method)
        .header("mcp-protocol-version", "2026-07-28");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(name) = params["name"].as_str().or(params["uri"].as_str()) {
        builder = builder.header("mcp-name", name);
    }
    builder
        .body(Body::from(
            json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string(),
        ))
        .unwrap()
}

async fn response(state: &Arc<State>, request: Request<Body>) -> (StatusCode, Value) {
    let response = cells::router(state.clone(), &[])
        .unwrap()
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn rpc(state: &Arc<State>, token: Option<&str>, method: &str, params: Value) -> Value {
    let (status, value) = response(state, request(token, method, params)).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert!(value.get("error").is_none(), "{value}");
    assert_eq!(value["result"]["resultType"], "complete");
    value["result"].clone()
}

async fn catalog(state: &Arc<State>, token: Option<&str>) -> Value {
    let list = rpc(state, token, "resources/list", json!({})).await;
    let value = rpc(
        state,
        token,
        "resources/read",
        json!({"uri":list["resources"][0]["uri"]}),
    )
    .await;
    assert_eq!(value["ttlMs"], 0);
    assert_eq!(value["cacheScope"], "private");
    assert_eq!(value["contents"][0]["mimeType"], "application/json");
    serde_json::from_str(value["contents"][0]["text"].as_str().unwrap()).unwrap()
}

async fn register(state: &Arc<State>, owner: &str, name: &str, path: &str, mounts: Value) -> Value {
    projects::call(
        state,
        state.auth.index(owner).unwrap(),
        "project_register",
        json!({"name":name,"alias":"docs","path":path,"access":"ro","mounts":mounts}),
    )
    .await
    .unwrap()["project"]
        .clone()
}

#[tokio::test]
async fn resources_are_discoverable_without_client_info_and_never_claim_subscriptions() {
    let (_root, state) = support::fixture();
    let discovery = rpc(&state, Some(ALICE), "server/discover", json!({})).await;
    assert_eq!(discovery["capabilities"]["resources"], json!({}));
    let list = rpc(&state, Some(ALICE), "resources/list", json!({})).await;
    assert_eq!(list["ttlMs"], 0);
    assert_eq!(list["cacheScope"], "private");
    assert_eq!(list["resources"].as_array().unwrap().len(), 1);
    assert_eq!(
        list["resources"][0]["uri"],
        "cells://directory-acl-test/directories"
    );
    assert_eq!(list["resources"][0]["name"], "accessible_directories");
    let templates = rpc(&state, Some(ALICE), "resources/templates/list", json!({})).await;
    assert_eq!(templates["resourceTemplates"], json!([]));
}

#[tokio::test]
async fn directory_catalog_filters_principals_and_projects_and_projects_mount_permissions() {
    let (root, state) = support::fixture();
    std::fs::create_dir_all(root.path().join("docs/project/ref")).unwrap();
    let alice = register(
        &state,
        "alice",
        "Alice project",
        "project",
        json!([{"alias":"private","path":"","at":"/workspace/ref","access":"ro"}]),
    )
    .await;
    register(&state, "bob", "Bob project", "project", json!([])).await;
    let value = catalog(&state, Some(ALICE)).await;
    assert_eq!(value["authorization"]["principalId"], "alice");
    assert_eq!(value["exports"].as_array().unwrap().len(), 2);
    assert_eq!(value["projects"].as_array().unwrap().len(), 1);
    assert_eq!(value["projects"][0]["id"], alice["id"]);
    assert_eq!(value["projects"][0]["revision"], alice["revision"]);
    check_project_mounts(&value["projects"][0]);
    for denied in [
        root.path().to_str().unwrap(),
        "rootIdentity",
        "mountIdentities",
        "secret.txt",
        "readme.txt",
        ALICE,
        BOB,
    ] {
        assert!(!value.to_string().contains(denied), "Leaked {denied}");
    }
    let bob = catalog(&state, Some(BOB)).await;
    assert_eq!(bob["exports"].as_array().unwrap().len(), 1);
    assert_eq!(bob["exports"][0]["access"], "ro");
    assert_eq!(bob["projects"][0]["name"], "Bob project");
    assert!(!bob.to_string().contains("private"));
    let public = catalog(&state, None).await;
    assert_eq!(public["projects"], json!([]));
    assert_eq!(public["workspaces"], json!([]));
    assert_eq!(
        public["exports"][0]["capabilities"],
        json!({"fileRead":true,"fileWrite":false,"run":false})
    );
}

fn check_project_mounts(project: &Value) {
    assert_eq!(project["root"]["virtualPath"], "/docs/project");
    assert_eq!(project["root"]["at"], "/workspace");
    assert_eq!(project["root"]["access"], "ro");
    assert_eq!(project["mounts"][0]["alias"], "private");
    assert_eq!(project["mounts"][0]["at"], "/workspace/ref");
    assert_eq!(project["mounts"][0]["capabilities"]["fileWrite"], false);
    assert_eq!(project["capabilities"]["run"], false);
}

#[tokio::test]
async fn directory_catalog_attenuates_saved_projects_after_a_grant_downgrade() {
    let (root, state) = support::fixture();
    let alice = state.auth.index("alice").unwrap();
    projects::call(
        &state,
        alice,
        "project_register",
        json!({"name":"Saved","alias":"docs","path":"","access":"rw"}),
    )
    .await
    .unwrap();
    assert_eq!(
        catalog(&state, Some(ALICE)).await["projects"][0]["root"]["access"],
        "rw"
    );
    drop(state);
    let mut config = support::config(root.path(), false);
    config.exports[0].grants[0].access = cells::config::Access::Ro;
    let state = State::from_config(&config).unwrap();
    let value = catalog(&state, Some(ALICE)).await;
    assert_eq!(value["exports"][0]["access"], "ro");
    assert_eq!(value["projects"][0]["root"]["access"], "ro");
    assert_eq!(value["projects"][0]["capabilities"]["fileWrite"], false);
}

#[tokio::test]
async fn resource_reads_respect_the_file_gate_without_hiding_failures() {
    let (_root, state) = support::fixture();
    let _guard = state.files.exclusive().unwrap();
    let (status, value) = response(
        &state,
        request(
            Some(ALICE),
            "resources/read",
            json!({"uri":"cells://directory-acl-test/directories"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(value["error"]["code"], -32603);
    assert_eq!(value["error"]["data"]["code"], "EBUSY");
    assert!(value.get("result").is_none());
}

#[tokio::test]
async fn directory_read_rechecks_authorization_and_replaced_project_roots() {
    let (root, state) = support::fixture();
    std::fs::create_dir(root.path().join("docs/project")).unwrap();
    register(&state, "alice", "Replaced", "project", json!([])).await;
    assert_eq!(
        catalog(&state, Some(ALICE)).await["projects"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    std::fs::rename(
        root.path().join("docs/project"),
        root.path().join("docs/previous"),
    )
    .unwrap();
    std::fs::create_dir(root.path().join("docs/project")).unwrap();
    assert_eq!(catalog(&state, Some(ALICE)).await["projects"], json!([]));
    let mut req = request(
        Some(ALICE),
        "resources/read",
        json!({"uri":"cells://directory-acl-test/directories"}),
    );
    req.headers_mut()
        .insert("x-cells-grant-revision", "stale".parse().unwrap());
    let (status, value) = response(&state, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(value["code"], "AUTHORIZATION_CHANGED");
    assert_eq!(
        response(&state, request(Some("wrong"), "resources/list", json!({})))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn resource_headers_and_unknown_uris_follow_json_rpc_errors() {
    let (_root, state) = support::fixture();
    let uri = "cells://directory-acl-test/directories";
    for header in [None, Some("other"), Some("=?base64?invalid!?=")] {
        let mut req = request(Some(ALICE), "resources/read", json!({"uri":uri}));
        req.headers_mut().remove("mcp-name");
        if let Some(value) = header {
            req.headers_mut().insert("mcp-name", value.parse().unwrap());
        }
        let (status, value) = response(&state, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["error"]["code"], -32020);
    }
    let mut encoded = request(Some(ALICE), "resources/read", json!({"uri":uri}));
    encoded.headers_mut().insert(
        "mcp-name",
        format!("=?base64?{}?=", STANDARD.encode(uri))
            .parse()
            .unwrap(),
    );
    assert_eq!(response(&state, encoded).await.0, StatusCode::OK);
    for uri in [
        "file:///etc/passwd",
        "cells://another/directories",
        "cells://directory-acl-test/../directories",
    ] {
        let (status, value) = response(
            &state,
            request(Some(ALICE), "resources/read", json!({"uri":uri})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(value["error"]["code"], -32602);
        assert!(value.get("result").is_none());
    }
}

#[tokio::test]
async fn resource_without_server_id_exposes_only_legacy_workspace_identifiers() {
    let root = tempfile::tempdir().unwrap();
    for directory in ["export", "home", "workspace"] {
        std::fs::create_dir(root.path().join(directory)).unwrap();
    }
    let config = toml::from_str(&format!(
        "listen='127.0.0.1:0'\nexecution=false\ntoken='{ALICE}'\n[[exports]]\nalias='docs'\npath={:?}\n[[harnesses]]\nid='legacy'\nkind='codex'\ncommand='/bin/false'\nhome={:?}\n[[harnesses.workspaces]]\nid='work'\npath={:?}\n",
        root.path().join("export"), root.path().join("home"), root.path().join("workspace"))).unwrap();
    let state = State::from_config(&config).unwrap();
    let list = rpc(&state, Some(ALICE), "resources/list", json!({})).await;
    assert_eq!(
        list["resources"][0]["uri"],
        format!(
            "cells://{}/directories",
            state.auth.server_id().unwrap()
        )
    );
    let value = catalog(&state, Some(ALICE)).await;
    assert_eq!(value["serverId"], state.auth.server_id().unwrap());
    assert_eq!(
        value["workspaces"],
        json!([{"profileId":"legacy","workspaceId":"work","capabilities":{"sessionCreate":true}}])
    );
    assert!(!value.to_string().contains(root.path().to_str().unwrap()));
}

#[tokio::test]
async fn tool_sets_follow_deployment_and_identity_with_precise_parameter_schemas() {
    let (_root, state) = support::fixture();
    let tools = rpc(&state, Some(ALICE), "tools/list", json!({})).await;
    let tools = tools["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "project_read"));
    assert!(!tools
        .iter()
        .any(|tool| tool["name"].as_str().unwrap().starts_with("harness_")));
    assert!(!tools
        .iter()
        .any(|tool| tool["name"].as_str().unwrap().starts_with("project_sync_")));
    let read = tools
        .iter()
        .find(|tool| tool["name"] == "project_read")
        .unwrap();
    assert_eq!(
        read["inputSchema"]["properties"].as_object().unwrap().len(),
        1
    );
    assert!(!tools
        .iter()
        .any(|tool| tool["name"] == "project_exec" || tool["name"] == "project_search"));
    let public = rpc(&state, None, "tools/list", json!({})).await;
    assert_eq!(public["tools"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_tool_requests_and_invalid_json_do_not_look_like_execution_results() {
    let (_root, state) = support::fixture();
    for params in [
        json!({"name":"unknown"}),
        json!({"name":"cells_capabilities","arguments":[]}),
    ] {
        let (status, value) = response(&state, request(Some(ALICE), "tools/call", params)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["error"]["code"], -32602);
        assert!(value.get("result").is_none());
    }
    let (status, value) = response(&state, request(Some(ALICE), "tools/call", json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(value["error"]["code"], -32020);
    let mut invalid = request(Some(ALICE), "ping", json!({}));
    *invalid.body_mut() = Body::from("{");
    let (status, value) = response(&state, invalid).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(value["error"]["code"], -32700);
    assert!(value.get("id").is_none());
}
