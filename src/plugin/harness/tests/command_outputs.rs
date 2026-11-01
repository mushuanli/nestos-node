//! Real service and MCP reads never manufacture a submission or reopen a native writer.
use super::fixture;
use axum::{body::Body, http::Request};
use base64::{engine::general_purpose::STANDARD, Engine};
use http_body_util::BodyExt;
use cells::{app::State, harness::Harnesses};
use serde_json::{json, Value};
use std::sync::{atomic::Ordering, Arc};
use tower::ServiceExt;

async fn mcp(state: &Arc<State>, name: &str, arguments: Value) -> Value {
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments,
        "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}});
    let request = Request::post("/mcp")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-method", "tools/call")
        .header("mcp-name", name)
        .header("mcp-protocol-version", "2026-07-28")
        .header("authorization", "Bearer fixture-api-key-at-least-24-bytes")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = cells::router(state.clone(), &[])
        .unwrap()
        .oneshot(request)
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let envelope: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(envelope.get("error").is_none(), "{envelope}");
    envelope["result"].clone()
}
fn read_args(reference: &Value) -> Value {
    let mut args = reference.clone();
    args["profileId"] = json!("custom");
    args["offset"] = json!(0);
    args["limit"] = json!(65536);
    args
}
fn command(epoch: &Value, id: &str) -> Value {
    json!({"profileId":"custom","epoch":epoch,"requestId":id,"sessionId":"fixture-session","command":"compact","arguments":"large","outputMode":"reference"})
}

#[tokio::test]
async fn legacy_full_results_and_negotiated_previews_share_one_native_execution() {
    let (_root, config, plugins, executions, _) = fixture();
    let state = State::from_config_with_plugins(&config, plugins).unwrap();
    let harness = state.services.require::<Harnesses>().unwrap();
    let mut args = command(&harness.profiles()["epoch"], "mode-once");
    args.as_object_mut().unwrap().remove("outputMode");
    let raw = mcp(&state, "harness_command", args.clone()).await["structuredContent"].clone();
    assert_eq!(raw["result"]["output"].as_str().unwrap().len(), 280000);
    assert!(raw.get("_commandOutput").is_none());
    assert!(raw["result"].get("outputRef").is_none());
    let mut referenced = args.clone();
    referenced["outputMode"] = json!("reference");
    let view =
        mcp(&state, "harness_command", referenced.clone()).await["structuredContent"].clone();
    assert!(view["result"]["output"].as_str().unwrap().len() <= 65536);
    assert!(view["result"]["outputRef"].is_object());
    assert!(view.get("_commandOutput").is_none());
    assert_eq!(
        mcp(&state, "harness_operation", args.clone()).await["structuredContent"],
        raw
    );
    assert_eq!(
        mcp(&state, "harness_operation", referenced).await["structuredContent"],
        view
    );
    for operation in ["harness_command", "harness_operation"] {
        for mode in [json!("raw"), json!("invalid"), Value::Null] {
            let mut invalid = args.clone();
            invalid["outputMode"] = mode;
            assert_eq!(mcp(&state, operation, invalid).await["isError"], true);
        }
    }
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    harness.close().await;
}

#[tokio::test]
async fn mcp_serves_scoped_unicode_bytes_and_missing_output_does_not_fence_future_execution() {
    let (_root, mut config, plugins, executions, _) = fixture();
    let mut other = config.harnesses[0].clone();
    other.id = "other".into();
    config.harnesses.push(other);
    let state = State::from_config_with_plugins(&config, plugins).unwrap();
    let harness = state.services.require::<Harnesses>().unwrap();
    let profiles = harness.profiles();
    assert_eq!(
        profiles["profiles"][0]["capabilities"]["commandOutputs"],
        true
    );
    let args = command(&profiles["epoch"], "output-once");
    let receipt = mcp(&state, "harness_command", args.clone()).await["structuredContent"].clone();
    let reference = &receipt["result"]["outputRef"];
    assert_eq!(receipt["outcome"], "committed");
    let read = read_args(reference);
    let reply = mcp(&state, "harness_command_output", read.clone()).await;
    assert_eq!(reply["isError"], false);
    let result = &reply["structuredContent"];
    assert_eq!(
        STANDARD
            .decode(result["data"].as_str().unwrap())
            .unwrap()
            .len(),
        65536
    );
    for (key, value) in [
        ("sha256", json!("0".repeat(64))),
        ("bytes", json!(0)),
        ("offset", json!(999999)),
        ("limit", json!(65537)),
        ("epoch", json!("foreign")),
        ("profileId", json!("other")),
        ("requestId", json!("missing-output")),
    ] {
        let mut invalid = read.clone();
        invalid[key] = value;
        assert_eq!(
            mcp(&state, "harness_command_output", invalid).await["isError"],
            true
        );
    }
    assert_eq!(
        mcp(&state, "harness_operation", args).await["structuredContent"],
        receipt
    );
    assert_eq!(
        mcp(
            &state,
            "harness_command",
            command(&profiles["epoch"], "missing-output")
        )
        .await["structuredContent"]["outcome"],
        "committed"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    harness.close().await;
}

async fn register(state: &Arc<State>, name: &str, path: &str) -> Value {
    cells::projects::call(
        state,
        0,
        "project_register",
        json!({"name":name,"alias":"work","path":path,"access":"rw"}),
    )
    .await
    .unwrap()["project"]
        .clone()
}

#[tokio::test]
async fn project_output_read_is_readonly_and_does_not_reopen_the_closed_project_driver() {
    let (root, config, plugins, executions, stops) = fixture();
    for name in ["a", "b"] {
        std::fs::create_dir(root.path().join("work").join(name)).unwrap();
    }
    let state = State::from_config_with_plugins(&config, plugins).unwrap();
    let harness = state.services.require::<Harnesses>().unwrap();
    let a = register(&state, "A", "a").await;
    let b = register(&state, "B", "b").await;
    let mut args = command(&harness.profiles()["epoch"], "project-output");
    args["projectId"] = a["id"].clone();
    args["revision"] = a["revision"].clone();
    let receipt = harness
        .project_call(&state, 0, "harness_command", args.clone())
        .await
        .unwrap();
    args.as_object_mut().unwrap().remove("outputMode");
    let raw = harness
        .project_call(&state, 0, "harness_command", args.clone())
        .await
        .unwrap();
    assert_eq!(raw["result"]["output"].as_str().unwrap().len(), 280000);
    assert!(raw.get("_commandOutput").is_none());
    assert_eq!(
        harness
            .project_call(&state, 0, "harness_operation", args)
            .await
            .unwrap(),
        raw
    );
    harness.close_project(a["id"].as_str().unwrap()).await;
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    let mut read = read_args(&receipt["result"]["outputRef"]);
    read["projectId"] = a["id"].clone();
    read["revision"] = a["revision"].clone();
    read["readOnly"] = json!(true);
    assert_eq!(
        mcp(&state, "harness_command_output", read.clone()).await["isError"],
        false
    );
    let mut foreign = read.clone();
    foreign["projectId"] = b["id"].clone();
    foreign["revision"] = b["revision"].clone();
    assert_eq!(
        mcp(&state, "harness_command_output", foreign).await["isError"],
        true
    );
    let mut stale = read;
    stale["revision"] = json!(999);
    assert_eq!(
        mcp(&state, "harness_command_output", stale).await["isError"],
        true
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    harness.close().await;
    assert_eq!(stops.load(Ordering::SeqCst), 2);
}
