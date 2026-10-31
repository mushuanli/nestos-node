#[path = "support/events.rs"]
mod event_support;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use nestos_node::{
    app::State,
    config::Config,
    harness::{config::ProfileConfig, Harnesses},
};
use serde_json::{json, Value};
use std::{os::unix::fs::PermissionsExt, path::Path};
use tower::ServiceExt;

fn config(root: &Path, command: &str) -> Config {
    toml::from_str(&format!(
        r#"
listen = "127.0.0.1:0"
execution = false
token = "test-harness-token-0123456789"
[[harnesses]]
id = "codex"
kind = "codex"
command = {command:?}
home = {root:?}
[[harnesses.workspaces]]
id = "project"
path = {root:?}
"#
    ))
    .unwrap()
}

fn service(root: &Path) -> Harnesses {
    let fixture = root.join("codex-fixture.py");
    std::fs::write(&fixture, include_str!("fixtures/codex-app-server.py")).unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = config(root, fixture.to_str().unwrap());
    Harnesses::new(
        nestos_node::harness::config::validate(&config).unwrap(),
        "epoch".into(),
    )
    .unwrap()
}

#[tokio::test]
async fn event_subscription_survives_idle_driver_replacement_without_pinning_the_process() {
    use futures_util::StreamExt;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("codex-fixture.py");
    std::fs::write(&executable, include_str!("fixtures/codex-app-server.py")).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = State::from_config(&config(root.path(), executable.to_str().unwrap())).unwrap();
    let service = state.services.require::<Harnesses>().unwrap();
    let mut source = service
        .subscribe(&state, 0, json!({"profileId":"codex"}))
        .await
        .unwrap();
    let first = source.next().await.unwrap().unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), source.next())
            .await
            .is_err()
    );
    service
        .reap_idle(std::time::Instant::now() + std::time::Duration::from_secs(301))
        .await;
    let epoch = service.profiles()["epoch"].clone();
    service.call("harness_create",json!({"profileId":"codex","epoch":epoch,"requestId":"after-reap","workspaceId":"project"})).await.unwrap();
    let next = tokio::time::timeout(std::time::Duration::from_secs(2), source.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_ne!(first["epoch"], next["epoch"]);
    drop(source);
    service.close().await;
}

async fn mutate(service: &Harnesses, name: &str, id: &str, extra: Value) -> Value {
    let mut args = json!({"profileId":"codex","epoch":"epoch","requestId":id});
    for (key, value) in extra.as_object().unwrap() {
        args[key] = value.clone();
    }
    service.call(name, args).await.unwrap()
}

#[tokio::test]
async fn reconciling_an_unreceived_request_fences_its_late_delivery() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let query = json!({"profileId":"codex","epoch":"epoch","requestId":"never-arrived"});
    let receipt = service
        .call("harness_operation", query.clone())
        .await
        .unwrap();
    assert_eq!(receipt["outcome"], "not-committed");
    assert_eq!(receipt["code"], "HARNESS_REQUEST_NOT_STARTED");
    let late = mutate(
        &service,
        "harness_create",
        "never-arrived",
        json!({"workspaceId":"project"}),
    )
    .await;
    assert_eq!(late, receipt);
    assert_eq!(
        service.call("harness_operation", query).await.unwrap(),
        receipt
    );
    let sessions = service
        .call(
            "harness_sessions",
            json!({"profileId":"codex","workspaceId":"project"}),
        )
        .await
        .unwrap();
    assert!(sessions["sessions"].as_array().unwrap().is_empty());
    service.close().await;
}

#[tokio::test]
async fn active_summary_refresh_recovers_a_long_answer_not_yet_flushed_to_history() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let created = mutate(
        &service,
        "harness_create",
        "long-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = &created["result"]["session"]["id"];
    mutate(
        &service,
        "harness_turn",
        "long-turn",
        json!({"sessionId":id,"prompt":"fixture:long-unflushed-response"}),
    )
    .await;
    let read = json!({"profileId":"codex","sessionId":id,"toolDetail":"summary"});
    let history = service
        .call("harness_session_read", read.clone())
        .await
        .unwrap();
    assert_eq!(history["turns"][0]["items"][0]["id"], "live-answer");
    assert_eq!(
        history["turns"][0]["items"][0]["text"],
        "Long response segment. ".repeat(8192)
    );
    assert_eq!(history["session"]["activeTurnId"], "turn-1");
    assert!(history["eventCursor"]["after"].as_u64().unwrap() > 1024);
    let events = event_support::snapshot(&service, json!({"profileId":"codex","after":history["eventCursor"]["after"],"eventEpoch":history["eventCursor"]["eventEpoch"]})).await.unwrap();
    assert_eq!(events["events"], json!([]));
    let reopened = service.call("harness_session_read", read).await.unwrap();
    assert_eq!(reopened["turns"], history["turns"]);
    service.close().await;
}

#[tokio::test]
async fn models_are_discovered_and_validated_before_native_turn_submission() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let catalog = service
        .call("harness_models", json!({"profileId":"codex"}))
        .await
        .unwrap();
    assert_eq!(catalog["models"][0]["reasoningEfforts"][1]["id"], "high");
    let created = mutate(
        &service,
        "harness_create",
        "model-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = &created["result"]["session"]["id"];
    let rejected = mutate(&service,"harness_turn","bad-effort",json!({"sessionId":id,"prompt":"check-model-selection","selection":{"modelId":"fixture-model","reasoningEffort":"invalid"}})).await;
    assert_eq!(rejected["outcome"], "not-committed");
    let accepted = mutate(&service,"harness_turn","model-turn",json!({"sessionId":id,"prompt":"check-model-selection","selection":{"modelId":"fixture-model","reasoningEffort":"high"}})).await;
    assert_eq!(accepted["outcome"], "committed");
    let current = service
        .call(
            "harness_models",
            json!({"profileId":"codex","sessionId":id}),
        )
        .await
        .unwrap();
    assert_eq!(current["current"]["reasoningEffort"], "high");
    service.close().await;
}

#[tokio::test]
async fn codex_commands_use_native_controls_and_keep_receipts() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let catalog = service
        .call("harness_commands", json!({"profileId":"codex"}))
        .await
        .unwrap();
    assert_eq!(catalog["commands"][0]["name"], "compact");
    let created = mutate(
        &service,
        "harness_create",
        "command-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = &created["result"]["session"]["id"];
    let observed = service.call("harness_query", json!({"profileId":"codex","sessionId":id,"command":"session:status","requestId":"query-state"})).await.unwrap();
    assert_eq!(observed["status"], "completed");
    assert!(service.call("harness_query", json!({"profileId":"codex","sessionId":id,"command":"compact","requestId":"invalid-query"})).await.is_err());
    let args = json!({"sessionId":id,"command":"compact"});
    let accepted = mutate(&service, "harness_command", "compact-once", args.clone()).await;
    assert_eq!(accepted["outcome"], "committed");
    assert_eq!(
        accepted,
        mutate(&service, "harness_command", "compact-once", args).await
    );
    let review = mutate(
        &service,
        "harness_command",
        "review",
        json!({"sessionId":id,"command":"review","arguments":"check types"}),
    )
    .await;
    assert_eq!(review["result"]["turnId"], "review-turn");
    let rejected = mutate(
        &service,
        "harness_command",
        "not-native",
        json!({"sessionId":id,"command":"theme"}),
    )
    .await;
    assert_eq!(rejected["outcome"], "not-committed");
    service.close().await;
}

#[tokio::test]
async fn codex_queries_expose_runtime_identity_and_reject_stale_command_bindings() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let created = mutate(
        &service,
        "harness_create",
        "status-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].clone();
    let started = mutate(
        &service,
        "harness_turn",
        "status-turn",
        json!({"sessionId":id,"prompt":"working"}),
    )
    .await;
    let turn = started["result"]["turnId"].clone();
    let status = mutate(
        &service,
        "harness_command",
        "status-query",
        json!({"sessionId":id,"command":"session:status"}),
    )
    .await;
    assert_eq!(status["outcome"], "committed");
    let state: Value = serde_json::from_str(status["result"]["output"].as_str().unwrap()).unwrap();
    assert_eq!(state["scope"], "controlled");
    assert_eq!(state["status"], "active");
    assert_eq!(state["activeTurnId"], turn);
    let rejected = mutate(
        &service,
        "harness_command",
        "stale-query",
        json!({"sessionId":id,"command":"session:status","expectedCommand":{"revision":"stale"}}),
    )
    .await;
    assert_eq!(rejected["code"], "HARNESS_COMMAND_CHANGED");
    let skills = service
        .call(
            "harness_commands",
            json!({"profileId":"codex","sessionId":id}),
        )
        .await
        .unwrap();
    assert!(skills["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["name"] == "skill:fixture-skill" && row["kind"] == "prompt"));
    mutate(
        &service,
        "harness_interrupt",
        "status-stop",
        json!({"sessionId":id,"turnId":turn}),
    )
    .await;
    service.close().await;
}

#[tokio::test]
async fn codex_summary_configuration_is_explicit_and_preserves_model_and_speed_defaults() {
    use nestos_node::harness::config::{CodexConfig, ReasoningSummary};
    for (summary, expected) in [(Some(ReasoningSummary::Auto), "auto"), (Some(ReasoningSummary::None), "none"), (None, "unset")] {
        let root = tempfile::tempdir().unwrap();
        let fixture = root.path().join("codex-fixture.py");
        std::fs::write(&fixture, include_str!("fixtures/codex-app-server.py")).unwrap();
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut settings = config(root.path(), fixture.to_str().unwrap());
        settings.harnesses[0].codex = summary.map(|reasoning_summary| CodexConfig {reasoning_summary: Some(reasoning_summary)});
        let service = Harnesses::new(nestos_node::harness::config::validate(&settings).unwrap(), "epoch".into()).unwrap();
        let created = mutate(&service, "harness_create", "create", json!({"workspaceId":"project"})).await;
        assert_eq!(created["outcome"], "committed");
        let result = mutate(&service, "harness_turn", "turn", json!({"sessionId":"session-1",
            "prompt":format!("fixture:checkpoint-summary-{expected}")})).await;
        assert_eq!(result["outcome"], "committed");
        service.close().await;
    }
}

#[tokio::test]
async fn cold_codex_history_recovers_rollout_terminals_without_adopting_the_session() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    std::fs::create_dir(root.path().join("sessions")).unwrap();
    std::fs::write(root.path().join("sessions/native.jsonl"), format!("{}\n{}\n",
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"original-turn"}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"original-turn"}}))).unwrap();
    std::fs::write(root.path().join("codex-fixture.py"), r#"#!/usr/bin/env python3
import json, os, sys
for line in sys.stdin:
    request = json.loads(line)
    if request.get('id') is None:
        continue
    method = request.get('method')
    if method not in ('initialize', 'thread/read'):
        raise RuntimeError('Unexpected native operation')
    home = os.environ['CODEX_HOME']
    result = {} if method == 'initialize' else {'thread': {'id':'native', 'cwd':home,
        'path':home+'/sessions/native.jsonl', 'status':{'type':'notLoaded'}, 'turns':[]}}
    print(json.dumps({'id':request['id'],'result':result}), flush=True)
"#).unwrap();
    let history = service.call("harness_session_read", json!({"profileId":"codex", "sessionId":"native", "toolDetail":"summary"})).await.unwrap();
    assert_eq!(history["turns"][1], json!({"id":"original-turn","status":"completed","items":[]}));
    assert_eq!(history["session"]["owned"], false);
    assert_eq!(history["session"]["status"], "notLoaded");
    assert!(history["session"]["lastTurnResult"].is_null());
    let revision = history["session"]["historyRevision"].as_str().unwrap().to_owned();
    assert_eq!(revision.len(), 64);
    let info = service.call("harness_session_info", json!({"profileId":"codex","sessionId":"native"})).await.unwrap();
    assert_eq!(info["session"]["historyRevision"], revision);
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(root.path().join("sessions/native.jsonl")).unwrap();
    writeln!(file, "{}", json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"text":"Latest external reply"}]}})).unwrap();
    let info = service.call("harness_session_info", json!({"profileId":"codex","sessionId":"native"})).await.unwrap();
    assert_ne!(info["session"]["historyRevision"], revision);
    let updated = service.call("harness_session_read", json!({"profileId":"codex","sessionId":"native","toolDetail":"summary"})).await.unwrap();
    assert_eq!(updated["turns"][0]["items"][0]["content"][0]["text"], "Latest external reply");
    assert_eq!(updated["session"]["historyRevision"], info["session"]["historyRevision"]);
    service.close().await;
}

#[tokio::test]
async fn creates_reads_resumes_streams_approves_and_interrupts_without_replay() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let created = mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    assert_eq!(created["outcome"], "committed");
    let repeated = mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    assert_eq!(created, repeated);
    let listed = service
        .call("harness_sessions", json!({"profileId":"codex"}))
        .await
        .unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["resumable"], true);
    assert_eq!(listed["sessions"][0]["createdAt"], 1791458500000i64);
    assert_eq!(listed["sessions"][0]["updatedAt"], 1791458565000i64);
    assert!(listed["sessions"][0]["branchName"].is_null());
    let history = service
        .call(
            "harness_session_read",
            json!({"profileId":"codex","sessionId":"session-1"}),
        )
        .await
        .unwrap();
    assert!(history["turns"].is_array());
    assert!(history["session"]["native"].get("turns").is_none());
    assert_eq!(
        mutate(
            &service,
            "harness_resume",
            "resume",
            json!({"sessionId":"session-1"})
        )
        .await["outcome"],
        "committed"
    );
    let turn = mutate(
        &service,
        "harness_turn",
        "turn",
        json!({"sessionId":"session-1","prompt":"test"}),
    )
    .await;
    assert_eq!(turn["result"]["turnId"], "turn-1");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let events = event_support::snapshot(&service, json!({"profileId":"codex"}))
        .await
        .unwrap();
    assert_eq!(events["requests"].as_array().unwrap().len(), 1);
    let approval = mutate(
        &service,
        "harness_respond",
        "approve",
        json!({"nativeRequestId":"approval-1","response":{"decision":"accept"}}),
    )
    .await;
    assert_eq!(approval["outcome"], "committed");
    assert_eq!(
        mutate(
            &service,
            "harness_interrupt",
            "interrupt",
            json!({"sessionId":"session-1","turnId":"turn-1"})
        )
        .await["outcome"],
        "committed"
    );
    assert_eq!(
        mutate(
            &service,
            "harness_turn",
            "turn",
            json!({"sessionId":"session-1","prompt":"test"})
        )
        .await,
        turn
    );
    service.close().await;
}

#[tokio::test]
async fn rejects_unauthorized_resume_foreign_turn_epoch_and_changed_id_payload() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let denied = mutate(
        &service,
        "harness_resume",
        "outside",
        json!({"sessionId":"foreign"}),
    )
    .await;
    assert_eq!(denied["code"], "EACCES");
    let denied = mutate(
        &service,
        "harness_turn",
        "foreign",
        json!({"sessionId":"foreign","prompt":"test"}),
    )
    .await;
    assert_eq!(denied["code"], "HARNESS_SESSION_NOT_OWNED");
    assert!(service
        .call(
            "harness_create",
            json!({"profileId":"codex","epoch":"old","requestId":"id","workspaceId":"project"})
        )
        .await
        .is_err());
    mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    assert_eq!(service.call("harness_create",json!({"profileId":"codex","epoch":"epoch","requestId":"create","workspaceId":"other"})).await.unwrap_err().code,"HARNESS_REQUEST_REUSED");
    service.close().await;
}

#[test]
fn rejects_profiles_overlapping_exclusive_exports() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config(root.path(), "codex");
    config.exports.push(nestos_node::config::ExportConfig {
        path: root.path().to_str().unwrap().into(),
        alias: Some("rw".into()),
        access: nestos_node::config::Access::Rw,
        public: false,
        grants: vec![],
    });
    assert!(nestos_node::harness::config::validate(&config).is_err());
    let mut duplicate: ProfileConfig = config.harnesses[0].clone();
    duplicate.id = "codex".into();
    config.harnesses.push(duplicate);
    assert!(nestos_node::harness::config::validate(&config).is_err());
}

fn body(method: &str) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":method,"params":{"_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientInfo":{"name":"test","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}}}})
}

#[tokio::test]
async fn mcp_auth_version_metadata_origin_and_discovery() {
    let root = tempfile::tempdir().unwrap();
    let state = State::from_config(&config(root.path(), "codex")).unwrap();
    let app = nestos_node::router(state, &[]).unwrap();
    let request = |auth: bool, version: &str, origin: Option<&str>| {
        let mut builder = Request::post("/mcp")
            .header("content-type", "application/json")
            .header("mcp-method", "server/discover")
            .header("mcp-protocol-version", version);
        if auth {
            builder = builder.header("authorization", "Bearer test-harness-token-0123456789");
        }
        if let Some(origin) = origin {
            builder = builder.header("origin", origin);
        }
        builder
            .body(Body::from(body("server/discover").to_string()))
            .unwrap()
    };
    assert_eq!(
        app.clone()
            .oneshot(request(false, "2026-07-28", None))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.clone()
            .oneshot(request(true, "2025-11-25", None))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.clone()
            .oneshot(request(true, "2026-07-28", Some("https://evil.example")))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = app
        .oneshot(request(true, "2026-07-28", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(result["result"]["supportedVersions"][0], "2026-07-28");
    assert_eq!(
        result["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "nestos-node"
    );
    assert_eq!(
        result["result"]["_meta"]["itookit/nestos-node"],
        result["result"]["_meta"]["itookit/pi-agent"]
    );
    assert_eq!(result["result"]["_meta"]["itookit/pi-agent"]["version"], 1);
    assert_eq!(
        result["result"]["_meta"]["itookit/pi-agent"]["harness"],
        true
    );
    assert_eq!(
        result["result"]["_meta"]["itookit/pi-agent"]["fileProtocol"],
        "fs-agent-http-v1"
    );
}

#[tokio::test]
async fn renamed_capability_tool_and_legacy_aliases_share_the_same_identity() {
    let root = tempfile::tempdir().unwrap();
    let app = nestos_node::router(
        State::from_config(&config(root.path(), "codex")).unwrap(),
        &[],
    )
    .unwrap();
    let mut descriptors = Vec::new();
    for name in [
        "nestos_node_capabilities",
        "piagent_capabilities",
        "fsagent_capabilities",
    ] {
        let mut request = body("tools/call");
        request["params"]["name"] = json!(name);
        request["params"]["arguments"] = json!({});
        let response = app
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-harness-token-0123456789")
                    .header("mcp-method", "tools/call")
                    .header("mcp-name", name)
                    .header("mcp-protocol-version", "2026-07-28")
                    .body(Body::from(request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert!(result.get("error").is_none());
        let descriptor = result["result"]["structuredContent"].clone();
        assert_eq!(descriptor["fileProtocol"], "fs-agent-http-v1");
        descriptors.push(descriptor);
    }
    assert_eq!(descriptors[0], descriptors[1]);
    assert_eq!(descriptors[0], descriptors[2]);
}

#[tokio::test]
async fn native_codex_creates_legacy_sessions_and_reads_unmaterialized_history() {
    if std::env::var("NESTOS_NODE_CODEX_TEST").as_deref() != Ok("1") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path(), "codex");
    let service = Harnesses::new(
        nestos_node::harness::config::validate(&config).unwrap(),
        "epoch".into(),
    )
    .unwrap();
    let sessions = service
        .call("harness_sessions", json!({"profileId":"codex"}))
        .await
        .unwrap();
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 0);
    let created = mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    assert_eq!(created["outcome"], "committed");
    assert_eq!(
        created["result"]["session"]["native"]["historyMode"],
        "legacy"
    );
    let id = created["result"]["session"]["id"].as_str().unwrap();
    let history = service
        .call(
            "harness_session_read",
            json!({"profileId":"codex","sessionId":id}),
        )
        .await
        .unwrap();
    assert_eq!(history["turns"], json!([]));
    assert_eq!(history["session"]["owned"], true);
    let compact = service
        .call(
            "harness_session_read",
            json!({"profileId":"codex","sessionId":id,"toolDetail":"summary"}),
        )
        .await
        .unwrap();
    assert_eq!(compact["turns"], json!([]));
    service.close().await;
}

#[tokio::test]
async fn shutdown_fences_new_harness_commands() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    service.close().await;
    assert_eq!(service.call("harness_create", json!({"profileId":"codex","epoch":"epoch","requestId":"after","workspaceId":"project"})).await.unwrap_err().code, "EIO");
}

#[tokio::test]
async fn approvals_can_unblock_a_turn_before_its_start_reply() {
    let root = tempfile::tempdir().unwrap();
    let service = std::sync::Arc::new(service(root.path()));
    mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let owner = service.clone();
    let turn = tokio::spawn(async move {
        mutate(
            &owner,
            "harness_turn",
            "turn",
            json!({"sessionId":"session-1","prompt":"approval-before-start-reply"}),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let events = event_support::snapshot(&service, json!({"profileId":"codex"}))
                .await
                .unwrap();
            if !events["requests"].as_array().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let reply = mutate(
            &service,
            "harness_respond",
            "approval",
            json!({"nativeRequestId":"approval-early","response":{"decision":"accept"}}),
        )
        .await;
        assert_eq!(reply["outcome"], "committed");
        assert_eq!(turn.await.unwrap()["outcome"], "committed");
    })
    .await
    .unwrap();
    service.close().await;
}

#[tokio::test]
async fn native_branches_keep_parent_identity_and_fork_receipts_prevent_duplicates() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    mutate(
        &service,
        "harness_create",
        "base",
        json!({"workspaceId":"project"}),
    )
    .await;
    let args = json!({"sessionId":"session-1","name":"Alternative"});
    let forked = mutate(&service, "harness_fork", "fork", args.clone()).await;
    assert_eq!(forked["outcome"], "committed");
    assert_eq!(forked["result"]["session"]["parentSessionId"], "session-1");
    assert_eq!(forked["result"]["session"]["title"], "Alternative");
    assert_eq!(forked["result"]["session"]["branchName"], "Alternative");
    assert_eq!(mutate(&service, "harness_fork", "fork", args).await, forked);
    let list = service
        .call("harness_sessions", json!({"profileId":"codex"}))
        .await
        .unwrap();
    assert_eq!(list["sessions"].as_array().unwrap().len(), 2);
    let denied = mutate(
        &service,
        "harness_fork",
        "denied",
        json!({"sessionId":"foreign"}),
    )
    .await;
    assert_eq!(denied["code"], "EACCES");
    service.close().await;
}

#[tokio::test]
async fn checkpoint_forks_copy_only_through_the_named_terminal_turn_and_reuse_receipts() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    mutate(
        &service,
        "harness_create",
        "base",
        json!({"workspaceId":"project"}),
    )
    .await;
    for id in ["one", "two"] {
        mutate(
            &service,
            "harness_turn",
            id,
            json!({"sessionId":"session-1","prompt":"fixture:checkpoint-turn"}),
        )
        .await;
    }
    let args = json!({"sessionId":"session-1","lastTurnId":"turn-1"});
    let forked = mutate(&service, "harness_fork", "checkpoint", args.clone()).await;
    assert_eq!(forked["outcome"], "committed");
    assert_eq!(
        forked["result"]["checkpoint"],
        json!({"sourceSessionId":"session-1","turnId":"turn-1","verified":true})
    );
    let child = forked["result"]["session"]["id"].as_str().unwrap();
    let history = service
        .call(
            "harness_session_read",
            json!({"profileId":"codex","sessionId":child}),
        )
        .await
        .unwrap();
    assert_eq!(history["turns"].as_array().unwrap().len(), 1);
    assert_eq!(history["turns"][0]["id"], "turn-1");
    assert_eq!(
        mutate(&service, "harness_fork", "checkpoint", args).await,
        forked
    );
    service.close().await;
}

#[tokio::test]
async fn checkpoint_forks_preserve_created_identity_when_native_ignores_the_boundary() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    mutate(
        &service,
        "harness_create",
        "base",
        json!({"workspaceId":"project"}),
    )
    .await;
    for id in ["one", "two"] {
        mutate(
            &service,
            "harness_turn",
            id,
            json!({"sessionId":"session-1","prompt":"fixture:checkpoint-turn"}),
        )
        .await;
    }
    mutate(
        &service,
        "harness_rename",
        "name",
        json!({"sessionId":"session-1","name":"fixture:ignore-checkpoint"}),
    )
    .await;
    let result = mutate(
        &service,
        "harness_fork",
        "checkpoint",
        json!({"sessionId":"session-1","lastTurnId":"turn-1"}),
    )
    .await;
    assert_eq!(result["outcome"], "committed");
    assert_eq!(result["result"]["checkpoint"]["verified"], false);
    assert_eq!(
        result["result"]["checkpoint"]["code"],
        "HARNESS_CHECKPOINT_BOUNDARY_MISMATCH"
    );
    assert_ne!(result["result"]["session"]["id"], "session-1");
    let denied = mutate(
        &service,
        "harness_fork",
        "missing",
        json!({"sessionId":"session-1","lastTurnId":"unknown"}),
    )
    .await;
    assert_eq!(denied["outcome"], "not-committed");
    assert_eq!(denied["code"], "HARNESS_CHECKPOINT_NOT_FOUND");
    service.close().await;
}

#[tokio::test]
async fn native_management_preserves_history_and_never_adopts_or_replays_mutations() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    mutate(
        &service,
        "harness_create",
        "create-management",
        json!({"workspaceId":"project"}),
    )
    .await;
    let args = json!({"sessionId":"session-1","name":"Renamed title"});
    let renamed = mutate(
        &service,
        "harness_rename",
        "rename-management",
        args.clone(),
    )
    .await;
    assert_eq!(renamed["result"]["session"]["title"], "Renamed title");
    assert_eq!(
        mutate(&service, "harness_rename", "rename-management", args).await,
        renamed
    );
    let outside = mutate(
        &service,
        "harness_rename",
        "outside-management",
        json!({"sessionId":"foreign","name":"Denied"}),
    )
    .await;
    assert_eq!(outside["code"], "EACCES");
    let archived = mutate(
        &service,
        "harness_archive",
        "archive-management",
        json!({"sessionId":"session-1"}),
    )
    .await;
    assert_eq!(archived["result"]["session"]["archived"], true);
    assert_eq!(archived["result"]["session"]["owned"], false);
    let page = service
        .call(
            "harness_sessions",
            json!({"profileId":"codex","archived":true}),
        )
        .await
        .unwrap();
    assert_eq!(page["sessions"].as_array().unwrap().len(), 1);
    let restored = mutate(
        &service,
        "harness_unarchive",
        "restore-management",
        json!({"sessionId":"session-1"}),
    )
    .await;
    assert_eq!(restored["result"]["session"]["archived"], false);
    assert_eq!(restored["result"]["session"]["owned"], false);
    let denied = mutate(
        &service,
        "harness_archive",
        "unowned-management",
        json!({"sessionId":"session-1"}),
    )
    .await;
    assert_eq!(denied["code"], "HARNESS_SESSION_NOT_OWNED");
    service.close().await;
}

#[tokio::test]
async fn permanent_deletion_checks_ownership_and_keeps_a_replay_safe_receipt() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    mutate(
        &service,
        "harness_create",
        "create-delete",
        json!({"workspaceId":"project"}),
    )
    .await;
    let denied = mutate(
        &service,
        "harness_delete",
        "delete-foreign",
        json!({"sessionId":"foreign"}),
    )
    .await;
    assert_eq!(denied["code"], "EACCES");
    mutate(
        &service,
        "harness_turn",
        "start-delete",
        json!({"sessionId":"session-1","prompt":"running"}),
    )
    .await;
    let busy = mutate(
        &service,
        "harness_delete",
        "delete-busy",
        json!({"sessionId":"session-1"}),
    )
    .await;
    assert_eq!(busy["code"], "EBUSY");
    mutate(
        &service,
        "harness_interrupt",
        "stop-delete",
        json!({"sessionId":"session-1","turnId":"turn-1"}),
    )
    .await;
    mutate(
        &service,
        "harness_archive",
        "archive-delete",
        json!({"sessionId":"session-1"}),
    )
    .await;
    let deleted = mutate(
        &service,
        "harness_delete",
        "delete",
        json!({"sessionId":"session-1"}),
    )
    .await;
    assert_eq!(deleted["outcome"], "committed");
    assert_eq!(deleted["result"]["deletedSessionIds"], json!(["session-1"]));
    assert_eq!(
        mutate(
            &service,
            "harness_delete",
            "delete",
            json!({"sessionId":"session-1"})
        )
        .await,
        deleted
    );
    let page = service
        .call(
            "harness_sessions",
            json!({"profileId":"codex","archived":true}),
        )
        .await
        .unwrap();
    assert_eq!(page["sessions"], json!([]));
    service.close().await;
}

#[tokio::test]
async fn native_codex_archives_unloaded_history_and_permanently_removes_it() {
    if std::env::var("NESTOS_NODE_CODEX_TEST").as_deref() != Ok("1") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let id = "019a0a02-1234-7000-8000-0123456789ab";
    let dir = root.path().join("sessions/2026/10/09");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-10-09T00-00-00-{id}.jsonl"));
    let metadata = json!({"timestamp":"2026-10-09T00:00:00Z","type":"session_meta","payload":{"id":id,"timestamp":"2026-10-09T00:00:00Z","cwd":root.path(),"originator":"codex_cli_rs","cli_version":"0.159.2","source":"cli","model_provider":"openai"}});
    let message = json!({"timestamp":"2026-10-09T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Temporary native deletion fixture"}]}});
    std::fs::write(&path, format!("{metadata}\n{message}\n")).unwrap();
    let config = config(root.path(), "codex");
    let service = Harnesses::new(
        nestos_node::harness::config::validate(&config).unwrap(),
        "epoch".into(),
    )
    .unwrap();
    let before = service
        .call(
            "harness_session_info",
            json!({"profileId":"codex","sessionId":id}),
        )
        .await
        .unwrap();
    assert_eq!(before["session"]["status"], "notLoaded");
    assert_eq!(before["session"]["owned"], false);
    let archived = mutate(
        &service,
        "harness_archive",
        "archive-native-delete",
        json!({"sessionId":id}),
    )
    .await;
    assert_eq!(archived["outcome"], "committed", "{archived}");
    assert!(!path.exists());
    let deleted = mutate(
        &service,
        "harness_delete",
        "delete-native",
        json!({"sessionId":id}),
    )
    .await;
    assert_eq!(deleted["outcome"], "committed", "{deleted}");
    assert_eq!(deleted["result"]["deletedSessionIds"], json!([id]));
    let sessions = service
        .call(
            "harness_sessions",
            json!({"profileId":"codex","archived":true}),
        )
        .await
        .unwrap();
    assert_eq!(sessions["sessions"], json!([]));
    assert!(service
        .call(
            "harness_session_info",
            json!({"profileId":"codex","sessionId":id})
        )
        .await
        .is_err());
    assert_eq!(
        mutate(
            &service,
            "harness_delete",
            "delete-native",
            json!({"sessionId":id})
        )
        .await,
        deleted
    );
    service.close().await;
}

#[tokio::test]
async fn permanent_deletion_rejects_foreign_spawned_descendants_before_commit() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    mutate(
        &service,
        "harness_create",
        "root",
        json!({"workspaceId":"project"}),
    )
    .await;
    mutate(
        &service,
        "harness_rename",
        "child-fixture",
        json!({"sessionId":"session-1","name":"fixture:foreign-child"}),
    )
    .await;
    let denied = mutate(
        &service,
        "harness_delete",
        "foreign-tree",
        json!({"sessionId":"session-1"}),
    )
    .await;
    assert_eq!(denied["outcome"], "not-committed");
    assert_eq!(denied["code"], "EACCES");
    let archived = mutate(
        &service,
        "harness_archive",
        "foreign-archive",
        json!({"sessionId":"session-1"}),
    )
    .await;
    assert_eq!(archived["outcome"], "not-committed");
    assert_eq!(archived["code"], "EACCES");
    let root = service
        .call(
            "harness_session_info",
            json!({"profileId":"codex","sessionId":"session-1"}),
        )
        .await
        .unwrap();
    assert_eq!(root["session"]["archived"], false);
    assert_eq!(root["session"]["owned"], true);
    service.close().await;
}

#[tokio::test]
async fn steering_uses_exact_active_turn_and_receipts_without_starting_another_turn() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let created = mutate(
        &service,
        "harness_create",
        "create-steer",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].as_str().unwrap();
    let started = mutate(
        &service,
        "harness_turn",
        "start-steer",
        json!({"sessionId":id,"prompt":"Start work"}),
    )
    .await;
    let turn = started["result"]["turnId"].as_str().unwrap();
    let args = json!({"sessionId":id,"turnId":turn,"prompt":"Additional direction"});
    let accepted = mutate(&service, "harness_steer", "steer-once", args.clone()).await;
    assert_eq!(accepted["result"]["turnId"], turn);
    assert_eq!(
        mutate(&service, "harness_steer", "steer-once", args).await,
        accepted
    );
    let receipt = service
        .call(
            "harness_operation",
            json!({"profileId":"codex","epoch":"epoch","requestId":"steer-once"}),
        )
        .await
        .unwrap();
    assert_eq!(receipt["outcome"], "committed");
    assert_eq!(receipt["result"]["turnId"], turn);
    let stale = service.call("harness_steer", json!({"profileId":"codex","epoch":"epoch","requestId":"stale-steer","sessionId":id,"turnId":"old","prompt":"Do not send elsewhere"})).await;
    assert_eq!(stale.unwrap()["outcome"], "not-committed");
    service.close().await;
}

#[tokio::test]
async fn steering_passes_a_submission_waiting_for_approval_without_releasing_it() {
    let root = tempfile::tempdir().unwrap();
    let service = std::sync::Arc::new(service(root.path()));
    let created = mutate(
        &service,
        "harness_create",
        "create-waiting-steer",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].clone();
    let (waiting_service, waiting_id) = (service.clone(), id.clone());
    let waiting = tokio::spawn(async move {
        mutate(
            &waiting_service,
            "harness_turn",
            "waiting-start",
            json!({"sessionId":waiting_id,"prompt":"approval-before-start-reply"}),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let page = event_support::snapshot(&service, json!({"profileId":"codex"}))
                .await
                .unwrap();
            if !page["requests"].as_array().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        mutate(
            &service,
            "harness_steer",
            "waiting-context",
            json!({"sessionId":id,"turnId":"turn-1","prompt":"Supplement during a tool approval"}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(result["outcome"], "committed");
    assert!(!waiting.is_finished());
    mutate(
        &service,
        "harness_respond",
        "waiting-approve",
        json!({"nativeRequestId":"approval-early","response":{"decision":"accept"}}),
    )
    .await;
    assert_eq!(waiting.await.unwrap()["outcome"], "committed");
    service.close().await;
}

#[tokio::test]
async fn native_metrics_separate_last_context_from_total_tokens_and_strip_account_identity() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let created = mutate(
        &service,
        "harness_create",
        "metrics-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].clone();
    mutate(
        &service,
        "harness_turn",
        "metrics-start",
        json!({"sessionId":id,"prompt":"fixture:metrics"}),
    )
    .await;
    let result = service
        .call(
            "harness_session_metrics",
            json!({"profileId":"codex","sessionId":id}),
        )
        .await
        .unwrap();
    assert_eq!(result["metrics"]["contextWindow"]["usedTokens"], 10000);
    assert_eq!(result["metrics"]["contextWindow"]["limitTokens"], 200000);
    assert_eq!(
        result["metrics"]["quota"]["windows"][0]["usedPercent"],
        25.0
    );
    assert_eq!(
        result["metrics"]["quota"]["windows"][0]["resetsAt"],
        1800000000000u64
    );
    assert!(!result.to_string().contains("private-account"));
    assert!(result["metrics"]["cost"].is_null());
    service.close().await;
}

#[tokio::test]
async fn session_digest_metadata_is_negotiated_and_does_not_resume_the_session() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    assert_eq!(service.profiles()["profiles"][0]["capabilities"]["sessionDigest"], true);
    let created = mutate(&service, "harness_create", "digest-create", json!({"workspaceId":"project"})).await;
    let id = created["result"]["session"]["id"].clone();
    let result = service.call("harness_session_digest", json!({"profileId":"codex","sessionId":id,"metadataOnly":true})).await.unwrap();
    assert_eq!(result["session"]["id"], id);
    assert_eq!(result["messages"], json!([]));
    assert!(result["session"].get("native").is_none());
    assert!(result["session"].get("usage").is_none());
    assert_eq!(result["revision"].as_str().unwrap().len(), 64);
    service.close().await;
}
