#[path = "dsh_command_output.rs"]
mod command_output;
#[path = "dsh_command_recovery.rs"]
mod command_recovery;
#[path = "support/events.rs"]
mod event_support;
#[path = "support/dsh.rs"]
mod fixture;
#[path = "dsh_preparation.rs"]
mod preparation;
use cells::{config::Config, harness::Harnesses};
use serde_json::{json, Value};

#[tokio::test]
async fn readonly_queries_bypass_native_discovery_and_never_create_mutation_receipts() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    host.state.errors.lock().unwrap().insert(
        "commands/list".into(),
        json!({"code":"session/writer-held","message":"private"}),
    );
    let service = service(root.path(), &host.url);
    let args = json!({"profileId":"dsh","sessionId":"existing","command":"session:status","requestId":"read-state"});
    let result = service.call("harness_query", args.clone()).await.unwrap();
    assert_eq!(result["commandId"], "read-state");
    assert_eq!(result["status"], "completed");
    let receipt = service
        .call(
            "harness_operation",
            json!({"profileId":"dsh","epoch":"epoch","requestId":"read-state"}),
        )
        .await
        .unwrap();
    assert_eq!(receipt["outcome"], "not-committed");
    for update in [
        json!({"command":"compact"}),
        json!({"arguments":"unsafe"}),
        json!({"expectedCommand":{"revision":"old"}}),
    ] {
        let mut invalid = args.clone();
        invalid
            .as_object_mut()
            .unwrap()
            .extend(update.as_object().unwrap().clone());
        assert!(service.call("harness_query", invalid).await.is_err());
    }
    assert!(!host.state.calls.lock().unwrap().iter().any(|row| matches!(
        row["method"].as_str(),
        Some("commands/list" | "commands/execute" | "session/create" | "session/prompt")
    )));
    service.close().await;
}

fn service(root: &std::path::Path, url: &str) -> Harnesses {
    service_options(root, url, "")
}

fn service_options(root: &std::path::Path, url: &str, options: &str) -> Harnesses {
    // The fixture login accepts a fixed token; production configuration uses an environment reference.
    let command = root.join("dsh-fixture");
    std::fs::write(
        &command,
        format!("#!/bin/sh\nprintf 'dsh web: {url}?token=fixture\\n'\nexec sleep 300\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config: Config = toml::from_str(&format!(
        r#"
listen="127.0.0.1:0"
token="test-dsh-token-0123456789"
execution=false
[[harnesses]]
id="dsh"
kind="dsh"
command={command:?}
home={root:?}
[[harnesses.workspaces]]
id="project"
path={root:?}
[harnesses.dsh]
{options}
"#
    ))
    .unwrap();
    Harnesses::new(
        cells::harness::config::validate(&config).unwrap(),
        "epoch".into(),
    )
    .unwrap()
}
async fn mutate(service: &Harnesses, name: &str, request: &str, args: Value) -> Value {
    let mut value = json!({"profileId":"dsh","epoch":"epoch","requestId":request});
    for (key, item) in args.as_object().unwrap() {
        value[key] = item.clone();
    }
    service.call(name, value).await.unwrap()
}
async fn events(service: &Harnesses) -> Value {
    event_support::snapshot(&service, json!({"profileId":"dsh"}))
        .await
        .unwrap()
}

#[tokio::test]
async fn model_routes_and_efforts_are_forwarded_before_prompt_admission() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let catalog = service
        .call(
            "harness_models",
            json!({"profileId":"dsh","sessionId":"existing"}),
        )
        .await
        .unwrap();
    assert_eq!(catalog["models"][0]["reasoningEfforts"][1]["id"], "high");
    mutate(
        &service,
        "harness_resume",
        "model-resume",
        json!({"sessionId":"existing"}),
    )
    .await;
    let bad = mutate(&service,"harness_turn","bad-model",json!({"sessionId":"existing","prompt":"check-model-selection","selection":{"modelId":catalog["models"][0]["id"],"reasoningEffort":"invalid"}})).await;
    assert_eq!(bad["outcome"], "not-committed");
    let accepted = mutate(&service,"harness_turn","model-turn",json!({"sessionId":"existing","prompt":"check-model-selection","selection":{"modelId":catalog["models"][0]["id"],"reasoningEffort":"high"}})).await;
    assert_eq!(accepted["outcome"], "committed");
    let calls = host.state.calls.lock().unwrap();
    let selected = calls
        .iter()
        .position(|call| call["method"] == "session/selectModel")
        .unwrap();
    let prompted = calls
        .iter()
        .position(|call| call["method"] == "session/prompt")
        .unwrap();
    assert!(selected < prompted);
    assert_eq!(
        calls[selected]["payload"]["args"]["request"]["provider"],
        "fixture"
    );
    assert_eq!(
        calls[selected]["payload"]["args"]["request"]["reasoningEffort"],
        "high"
    );
    drop(calls);
    service.close().await;
}
#[tokio::test]
async fn native_slash_commands_are_discovered_and_executed_without_prompting() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    mutate(
        &service,
        "harness_resume",
        "resume-command",
        json!({"sessionId":"existing"}),
    )
    .await;
    let catalog = service
        .call(
            "harness_commands",
            json!({"profileId":"dsh","sessionId":"existing"}),
        )
        .await
        .unwrap();
    assert_eq!(catalog["commands"][0]["name"], "compact");
    assert_eq!(catalog["commands"][0]["kind"], "action");
    let args = json!({"sessionId":"existing","command":"compact","arguments":"keep decisions"});
    let result = mutate(&service, "harness_command", "compact-once", args.clone()).await;
    assert_eq!(result["outcome"], "committed");
    assert_eq!(result["result"]["output"], "Context compacted");
    assert_eq!(
        result,
        mutate(&service, "harness_command", "compact-once", args).await
    );
    let calls = host.state.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["method"] == "commands/execute")
            .count(),
        1
    );
    assert!(!calls.iter().any(|call| call["method"] == "session/prompt"));
    drop(calls);
    service.close().await;
}

#[tokio::test]
async fn dsh_status_queries_do_not_adopt_or_prompt_an_external_running_session() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    host.state.rows.lock().unwrap().get_mut("existing").unwrap()["running"] = json!(true);
    let service = service(root.path(), &host.url);
    let result = mutate(
        &service,
        "harness_command",
        "query",
        json!({"sessionId":"existing","command":"session:status"}),
    )
    .await;
    assert_eq!(result["outcome"], "committed");
    let state: Value = serde_json::from_str(result["result"]["output"].as_str().unwrap()).unwrap();
    assert_eq!(state["scope"], "external");
    assert_eq!(state["status"], "active");
    assert_eq!(state["owned"], false);
    let calls = host.state.calls.lock().unwrap();
    assert!(!calls.iter().any(|call| matches!(
        call["method"].as_str(),
        Some("session/prompt" | "session/resume" | "commands/execute")
    )));
    drop(calls);
    service.close().await;
}

#[tokio::test]
async fn cold_command_discovery_never_invokes_the_native_agent_lookup() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    host.state.errors.lock().unwrap().insert(
        "commands/list".into(),
        json!({"code":"session/writer-held","message":"private native failure"}),
    );
    let service = service(root.path(), &host.url);
    let catalog = service
        .call(
            "harness_commands",
            json!({"profileId":"dsh","sessionId":"existing"}),
        )
        .await
        .unwrap();
    assert_eq!(
        catalog["observation"],
        json!({"complete":false,"code":"DSH_SESSION_NOT_ATTACHED"})
    );
    assert!(catalog["commands"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["execution"] == "query"));
    assert!(catalog["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["name"] == "session:status"));
    let status = mutate(
        &service,
        "harness_command",
        "cold-status",
        json!({"sessionId":"existing","command":"session:status"}),
    )
    .await;
    assert_eq!(status["outcome"], "committed");
    let calls = host.state.calls.lock().unwrap();
    assert!(!calls.iter().any(|row| matches!(
        row["method"].as_str(),
        Some("commands/list" | "commands/execute" | "session/prompt" | "session/create")
    )));
    drop(calls);
    service.close().await;
}

#[tokio::test]
async fn attached_command_catalog_busy_keeps_read_only_queries_available() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    mutate(
        &service,
        "harness_resume",
        "explicit-attach",
        json!({"sessionId":"existing"}),
    )
    .await;
    host.state
        .errors
        .lock()
        .unwrap()
        .insert("commands/list".into(), json!({"code":"session/busy"}));
    let catalog = service
        .call(
            "harness_commands",
            json!({"profileId":"dsh","sessionId":"existing"}),
        )
        .await
        .unwrap();
    assert_eq!(catalog["observation"]["complete"], false);
    let status = mutate(
        &service,
        "harness_command",
        "attached-status",
        json!({"sessionId":"existing","command":"session:status"}),
    )
    .await;
    assert_eq!(status["outcome"], "committed");
    assert!(!host
        .state
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|row| row["method"] == "commands/execute"));
    service.close().await;
}

#[tokio::test]
async fn creates_resumes_streams_and_answers_approvals_using_native_receipts() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let listed = service
        .call("harness_sessions", json!({"profileId":"dsh"}))
        .await
        .unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["id"], "existing");
    assert!(service
        .call(
            "harness_session_info",
            json!({"profileId":"dsh","sessionId":"private"})
        )
        .await
        .is_err());
    let created = mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].as_str().unwrap();
    assert!(
        created["result"]["session"].get("updatedAt").is_some(),
        "Session DTO requires an explicit nullable updatedAt for independent clients"
    );
    assert_eq!(
        mutate(
            &service,
            "harness_create",
            "create",
            json!({"workspaceId":"project"})
        )
        .await,
        created
    );
    let resumed = mutate(
        &service,
        "harness_resume",
        "resume",
        json!({"sessionId":"existing"}),
    )
    .await;
    assert_eq!(resumed["result"]["session"]["owned"], true);
    let turn = mutate(
        &service,
        "harness_turn",
        "turn-1",
        json!({"sessionId":id,"prompt":"hello"}),
    )
    .await;
    assert_eq!(turn["result"]["turnId"], "turn-1");
    host.emit(
        id,
        json!({"type":"event","event":{"seq":0,"type":"turn/start","data":{"turn":1}}}),
    );
    host.emit(id,json!({"type":"assistant-stream","frame":{"type":"start","attemptId":"attempt-1","revision":1}}));
    host.emit(id,json!({"type":"assistant-stream","frame":{"type":"chunk","revision":2,"index":0,"chunk":{"type":"text-delta","index":0,"text":"partial"}}}));
    host.event(json!({"type":"waterfall","event":"approval/request","eventId":"approval-1","agentId":id,"request":{"toolName":"shell","displayReason":"test"}}));
    for _ in 0..40 {
        if !events(&service).await["requests"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let page = events(&service).await;
    assert!(page["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["message"]["params"]["delta"] == "partial"));
    assert_eq!(page["requests"][0]["id"], "approval-1");
    mutate(
        &service,
        "harness_respond",
        "approve",
        json!({"nativeRequestId":"approval-1","response":{"decision":"accept"}}),
    )
    .await;
    assert!(events(&service).await["requests"]
        .as_array()
        .unwrap()
        .is_empty());
    mutate(
        &service,
        "harness_interrupt",
        "stop",
        json!({"sessionId":id,"turnId":"turn-1"}),
    )
    .await;
    host.emit(id,json!({"type":"event","event":{"seq":1,"type":"turn/end","data":{"turn":1,"reason":{"kind":"cancelled"}}}}));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let history = service
        .call(
            "harness_session_read",
            json!({"profileId":"dsh","sessionId":"existing"}),
        )
        .await
        .unwrap();
    assert_eq!(
        history["turns"][0]["items"][0]["content"][0]["text"],
        "older"
    );
    assert_eq!(
        host.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c["method"] == "session/create")
            .count(),
        1
    );
    service.close().await;
}
#[tokio::test]
async fn independent_admissions_and_streams_do_not_wait_for_another_session() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let a = mutate(
        &service,
        "harness_create",
        "a",
        json!({"workspaceId":"project"}),
    )
    .await;
    let b = mutate(
        &service,
        "harness_create",
        "b",
        json!({"workspaceId":"project"}),
    )
    .await;
    let a = a["result"]["session"]["id"].as_str().unwrap();
    let b = b["result"]["session"]["id"].as_str().unwrap();
    let slow = mutate(
        &service,
        "harness_turn",
        "slow",
        json!({"sessionId":a,"prompt":"slow-admission"}),
    );
    let fast = async {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        tokio::time::timeout(
            std::time::Duration::from_millis(300),
            mutate(
                &service,
                "harness_turn",
                "fast",
                json!({"sessionId":b,"prompt":"fast"}),
            ),
        )
        .await
        .unwrap()
    };
    let (_, result) = tokio::join!(slow, fast);
    assert_eq!(result["outcome"], "committed");
    service.close().await;
}
#[test]
fn rejects_external_servers_claiming_project_sandbox_support() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let config: Config = toml::from_str(&format!(
        r#"
listen="127.0.0.1:0"
token="test-dsh-token-0123456789"
[[harnesses]]
id="dsh"
kind="dsh"
home={root:?}
projects=true
[harnesses.dsh]
url="http://127.0.0.1:3080/"
"#
    ))
    .unwrap();
    assert!(cells::harness::config::validate(&config)
        .err()
        .unwrap()
        .contains("projects = false"));
}

#[tokio::test]
async fn uncertain_prompt_receipts_are_never_submitted_twice() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service_options(
        root.path(),
        &host.url,
        "request_timeout_ms=100\nmax_sessions=1",
    );
    let created = mutate(
        &service,
        "harness_create",
        "new",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = &created["result"]["session"]["id"];
    let args = json!({"sessionId":id,"prompt":"slow-admission"});
    let receipt = mutate(&service, "harness_turn", "uncertain", args.clone()).await;
    assert_eq!(receipt["outcome"], "unknown");
    assert_eq!(
        mutate(&service, "harness_turn", "uncertain", args).await,
        receipt
    );
    assert_eq!(
        mutate(
            &service,
            "harness_create",
            "blocked-while-unknown",
            json!({"workspaceId":"project"})
        )
        .await["code"],
        "DSH_SESSION_LIMIT"
    );
    assert_eq!(
        host.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == "session/prompt")
            .count(),
        1
    );
    service.close().await;
}
#[tokio::test]
async fn native_internal_errors_keep_the_operation_unknown_and_the_original_prompt_private() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let created = mutate(
        &service,
        "harness_create",
        "new",
        json!({"workspaceId":"project"}),
    )
    .await;
    host.state.errors.lock().unwrap().insert(
        "session/prompt".into(),
        json!({"code":"gateway/internal","message":"private upstream body"}),
    );
    let receipt = mutate(
        &service,
        "harness_turn",
        "failure",
        json!({"sessionId":created["result"]["session"]["id"],"prompt":"secret prompt"}),
    )
    .await;
    assert_eq!(receipt["outcome"], "unknown");
    assert!(!receipt.to_string().contains("private"));
    assert!(!receipt.to_string().contains("secret"));
    service.close().await;
}
#[tokio::test]
async fn session_capacity_is_reserved_before_native_creation() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service_options(root.path(), &host.url, "max_sessions=1");
    assert_eq!(
        mutate(
            &service,
            "harness_create",
            "one",
            json!({"workspaceId":"project"})
        )
        .await["outcome"],
        "committed"
    );
    let second = mutate(
        &service,
        "harness_create",
        "two",
        json!({"workspaceId":"project"}),
    )
    .await;
    assert_eq!(second["outcome"], "not-committed");
    assert_eq!(second["code"], "DSH_SESSION_LIMIT");
    assert_eq!(
        host.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == "session/create")
            .count(),
        1
    );
    service.close().await;
}
#[tokio::test]
async fn history_pages_keep_the_opening_watermark_when_new_messages_arrive() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    host.state.records.lock().unwrap().insert("existing".into(),(0..151).map(|seq|json!({"type":"event","event":{
        "seq":seq,"type":"user/message","data":{"content":[{"type":"text","text":format!("message {seq}")}]}}})).collect());
    let service = service(root.path(), &host.url);
    let first = service
        .call(
            "harness_session_read",
            json!({"profileId":"dsh","sessionId":"existing"}),
        )
        .await
        .unwrap();
    assert_eq!(first["turns"][0]["items"].as_array().unwrap().len(), 80);
    let older = service
        .call(
            "harness_session_read",
            json!({"profileId":"dsh","sessionId":"existing","cursor":first["nextCursor"]}),
        )
        .await
        .unwrap();
    assert_eq!(older["turns"][0]["items"].as_array().unwrap().len(), 71);
    assert!(older["nextCursor"].is_null());
    assert!(host
        .state
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c["method"] == "session/page"
            && c["payload"]["args"]["request"]["throughSeq"] == 150));
    service.close().await;
}

#[tokio::test]
async fn thirty_two_sessions_admit_work_and_deliver_deltas_independently() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let created = futures_util::future::join_all((0..32).map(|i| {
        let service = &service;
        async move {
            mutate(
                service,
                "harness_create",
                &format!("create-{i}"),
                json!({"workspaceId":"project"}),
            )
            .await
        }
    }))
    .await;
    let ids = created
        .iter()
        .map(|result| {
            result["result"]["session"]["id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    let started = futures_util::future::join_all(ids.iter().enumerate().map(|(i, id)| {
        let service = &service;
        async move {
            mutate(
                service,
                "harness_turn",
                &format!("turn-{i}"),
                json!({"sessionId":id,"prompt":"parallel"}),
            )
            .await
        }
    }))
    .await;
    assert!(started
        .iter()
        .all(|result| result["outcome"] == "committed"));
    for (i, id) in ids.iter().enumerate() {
        host.emit(
            id,
            json!({"type":"event","event":{"seq":0,"type":"turn/start","data":{"turn":1}}}),
        );
        host.emit(id,json!({"type":"assistant-stream","frame":{"type":"start","attemptId":format!("attempt-{i}"),"revision":1}}));
        host.emit(id,json!({"type":"assistant-stream","frame":{"type":"chunk","revision":2,"index":0,"chunk":{"type":"text-delta","index":0,"text":"response"}}}));
    }
    let mut received = 0;
    for _ in 0..100 {
        received = events(&service).await["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["message"]["params"]["delta"] == "response")
            .count();
        if received == 32 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(received, 32);
    service.close().await;
}

#[tokio::test]
async fn searches_authorized_titles_and_prompts_beyond_loaded_history_pages() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    host.state.rows.lock().unwrap().get_mut("existing").unwrap()["projections"] =
        json!({"values":{"title":"Older session"}});
    host.state.records.lock().unwrap().insert("existing".into(), (0..151).map(|seq| json!({"type":"event","event":{
        "seq":seq,"type":"user/message","data":{"content":[{"type":"text","text":format!("message {seq}")}]}}})).collect());
    let service = service(root.path(), &host.url);
    assert_eq!(
        service.profiles()["profiles"][0]["capabilities"]["searchPrompts"],
        true
    );
    for (query, mode, count) in [
        ("session　OLDER", "title", 1),
        ("0 message", "prompt", 25),
        ("0 message", "content", 25),
        ("message absent", "content", 0),
    ] {
        let result = service
            .call(
                "harness_session_search",
                json!({"profileId":"dsh","query":query,"mode":mode}),
            )
            .await
            .unwrap();
        assert_eq!(
            result["matches"].as_array().unwrap().len(),
            count,
            "{result}"
        );
        if count > 0 {
            assert_eq!(result["matches"][0]["sessionId"], "existing");
        }
        assert_eq!(result["truncated"], false);
    }
    assert!(host
        .state
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|call| call["method"] == "session/page"));
    service.close().await;
}

#[tokio::test]
async fn completed_sessions_release_observation_capacity_without_archiving_native_history() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service_options(root.path(), &host.url, "max_sessions=1");
    let first = mutate(
        &service,
        "harness_create",
        "capacity-first",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = first["result"]["session"]["id"].as_str().unwrap();
    mutate(
        &service,
        "harness_turn",
        "capacity-turn",
        json!({"sessionId":id,"prompt":"complete"}),
    )
    .await;
    host.emit(
        id,
        json!({"type":"event","event":{"seq":0,"type":"turn/start","data":{"turn":1}}}),
    );
    host.emit(id, json!({"type":"event","event":{"seq":1,"type":"assistant/message","data":{"turn":1,"content":[{"type":"text","text":"retained result"}]}}}));
    host.emit(id, json!({"type":"event","event":{"seq":2,"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}));
    wait_terminal(&service, id).await;
    let next = mutate(
        &service,
        "harness_create",
        "capacity-next",
        json!({"workspaceId":"project"}),
    )
    .await;
    assert_eq!(
        next["outcome"], "committed",
        "completed observation must not consume capacity: {next}"
    );
    let history = service
        .call(
            "harness_session_read",
            json!({"profileId":"dsh","sessionId":id}),
        )
        .await
        .unwrap();
    assert_eq!(history["session"]["owned"], false);
    assert!(history["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|turn| turn["items"].as_array().unwrap())
        .any(|item| item["text"] == "retained result"));
    let receipt = service
        .call(
            "harness_operation",
            json!({"profileId":"dsh","requestId":"capacity-turn","epoch":"epoch"}),
        )
        .await
        .unwrap();
    assert_eq!(receipt["outcome"], "committed");
    let next_id = next["result"]["session"]["id"].as_str().unwrap();
    mutate(
        &service,
        "harness_turn",
        "capacity-next-turn",
        json!({"sessionId":next_id,"prompt":"complete"}),
    )
    .await;
    host.emit(
        next_id,
        json!({"type":"event","event":{"seq":0,"type":"turn/start","data":{"turn":1}}}),
    );
    host.emit(next_id, json!({"type":"event","event":{"seq":1,"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}));
    wait_terminal(&service, next_id).await;
    let restored = mutate(
        &service,
        "harness_resume",
        "capacity-restore",
        json!({"sessionId":id}),
    )
    .await;
    assert_eq!(restored["outcome"], "committed");
    assert_eq!(restored["result"]["session"]["id"], id);
    assert_eq!(restored["result"]["session"]["owned"], true);
    let calls = host.state.calls.lock().unwrap();
    assert!(!calls.iter().any(|row| matches!(
        row["method"].as_str(),
        Some("session/cancel" | "workspace/archiveSession")
    )));
    drop(calls);
    service.close().await;
}
async fn wait_terminal(service: &Harnesses, id: &str) {
    for _ in 0..100 {
        let page = events(service).await;
        if page["events"].as_array().unwrap().iter().any(|row| {
            row["message"]["method"] == "turn/completed"
                && row["message"]["params"]["threadId"] == id
        }) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("native terminal event was not observed");
}

#[tokio::test]
async fn unknown_serial_commands_preserve_ownership_and_block_further_side_effects() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service_options(root.path(), &host.url, "max_sessions=1");
    let first = mutate(
        &service,
        "harness_create",
        "unknown-command-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = first["result"]["session"]["id"].as_str().unwrap();
    mutate(
        &service,
        "harness_turn",
        "before-command",
        json!({"sessionId":id,"prompt":"completed"}),
    )
    .await;
    host.emit(
        id,
        json!({"type":"event","event":{"seq":0,"type":"turn/start","data":{"turn":1}}}),
    );
    host.emit(id, json!({"type":"event","event":{"seq":1,"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}));
    wait_terminal(&service, id).await;
    host.state.errors.lock().unwrap().insert(
        "commands/execute".into(),
        json!({"code":"gateway/internal","message":"private-command-detail"}),
    );
    let receipt = mutate(
        &service,
        "harness_command",
        "unknown-command",
        json!({"sessionId":id,"command":"compact"}),
    )
    .await;
    assert_eq!(receipt["outcome"], "unknown");
    assert!(!receipt.to_string().contains("private-command-detail"));
    assert_eq!(
        mutate(
            &service,
            "harness_create",
            "after-unknown-command",
            json!({"workspaceId":"project"})
        )
        .await["code"],
        "DSH_SESSION_LIMIT"
    );
    assert_eq!(
        mutate(
            &service,
            "harness_turn",
            "blocked-turn",
            json!({"sessionId":id,"prompt":"do not dispatch"})
        )
        .await["code"],
        "DSH_COMMAND_RESULT_PENDING"
    );
    let query = mutate(
        &service,
        "harness_command",
        "query-unknown-command",
        json!({"sessionId":id,"command":"session:status"}),
    )
    .await;
    assert_eq!(query["outcome"], "committed");
    assert_eq!(
        mutate(
            &service,
            "harness_command",
            "unknown-command",
            json!({"sessionId":id,"command":"compact"})
        )
        .await,
        receipt
    );
    let calls = host.state.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|row| row["method"] == "commands/execute")
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|row| row["method"] == "session/prompt")
            .count(),
        1
    );
    drop(calls);
    service.close().await;
}

#[tokio::test]
async fn durable_prompt_identity_resolves_an_unknown_receipt_without_replaying_the_prompt() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service_options(root.path(), &host.url, "max_sessions=1");
    let created = mutate(
        &service,
        "harness_create",
        "proof-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].as_str().unwrap();
    host.state.errors.lock().unwrap().insert(
        "session/prompt".into(),
        json!({"code":"gateway/internal","message":"private native failure"}),
    );
    let args = json!({"sessionId":id,"prompt":"Exact frozen input"});
    assert_eq!(
        mutate(&service, "harness_turn", "proof-request", args.clone()).await["outcome"],
        "unknown"
    );
    let query = json!({"profileId":"dsh","requestId":"proof-request","epoch":"epoch"});
    assert_eq!(
        service
            .call("harness_operation", query.clone())
            .await
            .unwrap()["outcome"],
        "unknown"
    );
    host.emit(
        id,
        json!({"type":"event","event":{"seq":0,"type":"turn/start","data":{"turn":1}}}),
    );
    host.emit(id, json!({"type":"event","event":{"seq":1,"type":"user/message","data":{"turn":1,"source":{"kind":"user","rpcId":"proof-request"},"content":[{"type":"text","text":"Exact frozen input"}]}}}));
    host.emit(id, json!({"type":"event","event":{"seq":2,"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}));
    wait_terminal(&service, id).await;
    assert_eq!(
        mutate(
            &service,
            "harness_create",
            "before-proof-persisted",
            json!({"workspaceId":"project"})
        )
        .await["code"],
        "DSH_SESSION_LIMIT"
    );
    let receipt = service
        .call("harness_operation", query.clone())
        .await
        .unwrap();
    assert_eq!(receipt["outcome"], "committed");
    assert_eq!(receipt["result"]["turnId"], "proof-request");
    assert_eq!(
        mutate(
            &service,
            "harness_create",
            "after-proof-persisted",
            json!({"workspaceId":"project"})
        )
        .await["outcome"],
        "committed"
    );
    assert_eq!(
        service.call("harness_operation", query).await.unwrap(),
        receipt
    );
    assert_eq!(
        mutate(&service, "harness_turn", "proof-request", args).await,
        receipt
    );
    assert_eq!(
        host.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row["method"] == "session/prompt")
            .count(),
        1
    );
    service.close().await;
}

#[tokio::test]
async fn unrelated_or_changed_native_input_cannot_confirm_an_unknown_prompt() {
    for (source, content) in [
        (
            json!({"kind":"user","rpcId":"another-request"}),
            "Exact frozen input",
        ),
        (
            json!({"kind":"user","rpcId":"frozen-request"}),
            "Changed input",
        ),
        (
            json!({"kind":"injected","rpcId":"frozen-request"}),
            "Exact frozen input",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let host = fixture::Host::new(root.path()).await;
        host.state
            .identify_prompts
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let service = service_options(root.path(), &host.url, "max_sessions=1");
        let created = mutate(
            &service,
            "harness_create",
            "different-proof-create",
            json!({"workspaceId":"project"}),
        )
        .await;
        let id = created["result"]["session"]["id"].as_str().unwrap();
        host.state.errors.lock().unwrap().insert(
            "session/prompt".into(),
            json!({"code":"gateway/internal","message":"private"}),
        );
        mutate(
            &service,
            "harness_turn",
            "frozen-request",
            json!({"sessionId":id,"prompt":"Exact frozen input"}),
        )
        .await;
        host.emit(
            id,
            json!({"type":"event","event":{"seq":0,"type":"turn/start","data":{"turn":1}}}),
        );
        host.emit(id, json!({"type":"event","event":{"seq":1,"type":"user/message","data":{"turn":1,"source":source,"content":[{"type":"text","text":content}]}}}));
        host.emit(id, json!({"type":"event","event":{"seq":2,"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}));
        wait_native_input(&service, id, content).await;
        let result = service
            .call(
                "harness_operation",
                json!({"profileId":"dsh","requestId":"frozen-request","epoch":"epoch"}),
            )
            .await;
        if content == "Changed input" {
            assert_eq!(result.err().unwrap().code, "DSH_PROMPT_EVIDENCE_MISMATCH");
        } else {
            let receipt = result.unwrap();
            assert_eq!(receipt["outcome"], "unknown");
            assert!(!receipt.to_string().contains(content));
        }
        assert_eq!(
            mutate(
                &service,
                "harness_create",
                "still-uncertain",
                json!({"workspaceId":"project"})
            )
            .await["code"],
            "DSH_SESSION_LIMIT"
        );
        service.close().await;
    }
}

#[tokio::test]
async fn a_late_submission_rejection_cannot_clear_the_next_accepted_turn() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let created = mutate(
        &service,
        "harness_create",
        "race-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].as_str().unwrap();
    let original = mutate(
        &service,
        "harness_turn",
        "slow-original",
        json!({"sessionId":id,"prompt":"late-reject-admission"}),
    );
    let progress = async {
        for _ in 0..100 {
            if host
                .state
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|row| row["method"] == "session/prompt")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        host.emit(
            id,
            json!({"type":"event","event":{"seq":0,"type":"turn/start","data":{"turn":1}}}),
        );
        host.emit(id, json!({"type":"event","event":{"seq":1,"type":"user/message","data":{"turn":1,"source":{"kind":"user","rpcId":"slow-original"},"content":[{"type":"text","text":"late-reject-admission"}]}}}));
        host.emit(id, json!({"type":"event","event":{"seq":2,"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}));
        wait_terminal(&service, id).await;
        let receipt = service
            .call(
                "harness_operation",
                json!({"profileId":"dsh","epoch":"epoch","requestId":"slow-original"}),
            )
            .await
            .unwrap();
        assert_eq!(receipt["outcome"], "committed");
        let next = mutate(
            &service,
            "harness_turn",
            "following-turn",
            json!({"sessionId":id,"prompt":"Following input"}),
        )
        .await;
        assert_eq!(next["outcome"], "committed");
        receipt
    };
    let (late, confirmed) = tokio::join!(original, progress);
    assert_eq!(late, confirmed);
    let session = service
        .call(
            "harness_session_info",
            json!({"profileId":"dsh","sessionId":id}),
        )
        .await
        .unwrap();
    assert_eq!(session["session"]["activeTurnId"], "following-turn");
    service.close().await;
}

async fn wait_native_input(service: &Harnesses, id: &str, text: &str) {
    for _ in 0..100 {
        let page = events(service).await;
        if page["events"].as_array().unwrap().iter().any(|row| {
            row["message"]["params"]["threadId"] == id
                && row["message"]["params"]["item"]["content"][0]["text"] == text
        }) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("native input event was not observed");
}

#[tokio::test]
async fn cancel_requires_identified_input_and_preserves_a_racing_success() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let created = mutate(
        &service,
        "harness_create",
        "cancel-proof-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].as_str().unwrap();
    mutate(
        &service,
        "harness_turn",
        "cancel-proof-turn",
        json!({"sessionId":id,"prompt":"identified input"}),
    )
    .await;
    assert_eq!(
        mutate(
            &service,
            "harness_interrupt",
            "too-early",
            json!({"sessionId":id,"turnId":"cancel-proof-turn"})
        )
        .await["code"],
        "DSH_TURN_NOT_IDENTIFIED"
    );
    assert!(!host
        .state
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|row| row["method"] == "session/cancel"));
    host.emit(
        id,
        json!({"type":"event","event":{"type":"turn/start","data":{"turn":1}}}),
    );
    wait_native_input(&service, id, "identified input").await;
    assert_eq!(
        mutate(
            &service,
            "harness_interrupt",
            "identified-stop",
            json!({"sessionId":id,"turnId":"cancel-proof-turn"})
        )
        .await["outcome"],
        "committed"
    );
    host.emit(id, json!({"type":"event","event":{"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}));
    wait_terminal(&service, id).await;
    let page = events(&service).await;
    assert!(page["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["message"]["method"] == "turn/completed"
            && row["message"]["params"]["turn"]["status"] == "completed"));
    service.close().await;
}

#[tokio::test]
async fn an_unknown_cancel_retains_capacity_after_the_target_finishes() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service_options(root.path(), &host.url, "max_sessions=1");
    let created = mutate(
        &service,
        "harness_create",
        "unknown-cancel-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].as_str().unwrap();
    mutate(
        &service,
        "harness_turn",
        "unknown-cancel-turn",
        json!({"sessionId":id,"prompt":"cancel target"}),
    )
    .await;
    host.emit(
        id,
        json!({"type":"event","event":{"type":"turn/start","data":{"turn":1}}}),
    );
    wait_native_input(&service, id, "cancel target").await;
    host.state.errors.lock().unwrap().insert(
        "session/cancel".into(),
        json!({"code":"gateway/internal","message":"private"}),
    );
    let args = json!({"sessionId":id,"turnId":"unknown-cancel-turn"});
    let cancel = mutate(
        &service,
        "harness_interrupt",
        "unknown-cancel",
        args.clone(),
    )
    .await;
    assert_eq!(cancel["outcome"], "unknown");
    host.emit(id, json!({"type":"event","event":{"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}));
    wait_terminal(&service, id).await;
    assert_eq!(
        mutate(
            &service,
            "harness_create",
            "blocked-unknown-cancel",
            json!({"workspaceId":"project"})
        )
        .await["code"],
        "DSH_SESSION_LIMIT"
    );
    assert_eq!(
        mutate(
            &service,
            "harness_turn",
            "blocked-by-control",
            json!({"sessionId":id,"prompt":"do not dispatch"})
        )
        .await["code"],
        "DSH_CONTROL_RESULT_PENDING"
    );
    assert_eq!(
        mutate(&service, "harness_interrupt", "unknown-cancel", args).await,
        cancel
    );
    assert_eq!(
        host.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row["method"] == "session/cancel")
            .count(),
        1
    );
    service.close().await;
}

#[tokio::test]
async fn a_late_rejection_cannot_detach_the_still_running_identified_turn() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let created = mutate(
        &service,
        "harness_create",
        "running-race-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].as_str().unwrap();
    let original = mutate(
        &service,
        "harness_turn",
        "running-original",
        json!({"sessionId":id,"prompt":"late-reject-admission"}),
    );
    let identify = async {
        for _ in 0..100 {
            if host
                .state
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|row| row["method"] == "session/prompt")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        host.emit(
            id,
            json!({"type":"event","event":{"type":"turn/start","data":{"turn":1}}}),
        );
        wait_native_input(&service, id, "late-reject-admission").await;
        let receipt = service
            .call(
                "harness_operation",
                json!({"profileId":"dsh","epoch":"epoch","requestId":"running-original"}),
            )
            .await
            .unwrap();
        assert_eq!(receipt["outcome"], "committed");
        receipt
    };
    let (late, confirmed) = tokio::join!(original, identify);
    assert_eq!(late, confirmed);
    let snapshot = service
        .call(
            "harness_session_info",
            json!({"profileId":"dsh","sessionId":id}),
        )
        .await
        .unwrap();
    assert_eq!(snapshot["session"]["activeTurnId"], "running-original");
    host.emit(id, json!({"type":"event","event":{"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}));
    wait_terminal(&service, id).await;
    service.close().await;
}

#[tokio::test]
async fn a_foreign_turn_cannot_complete_or_cancel_the_uncertain_owned_input() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    host.state
        .identify_prompts
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let service = service(root.path(), &host.url);
    let created = mutate(
        &service,
        "harness_create",
        "foreign-first-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].as_str().unwrap();
    host.state.errors.lock().unwrap().insert(
        "session/prompt".into(),
        json!({"code":"gateway/internal","message":"private"}),
    );
    assert_eq!(
        mutate(
            &service,
            "harness_turn",
            "owned-input",
            json!({"sessionId":id,"prompt":"owned text"})
        )
        .await["outcome"],
        "unknown"
    );
    host.emit(
        id,
        json!({"type":"event","event":{"type":"turn/start","data":{"turn":1}}}),
    );
    host.emit(id, json!({"type":"event","event":{"type":"user/message","data":{"turn":1,"source":{"kind":"user","rpcId":"external-input"},"content":[{"type":"text","text":"foreign text"}]}}}));
    wait_native_input(&service, id, "foreign text").await;
    assert_eq!(
        mutate(
            &service,
            "harness_interrupt",
            "do-not-cancel-foreign",
            json!({"sessionId":id,"turnId":"owned-input"})
        )
        .await["code"],
        "DSH_TURN_NOT_IDENTIFIED"
    );
    host.emit(id, json!({"type":"event","event":{"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}));
    let query = json!({"profileId":"dsh","epoch":"epoch","requestId":"owned-input"});
    assert_eq!(
        service
            .call("harness_operation", query.clone())
            .await
            .unwrap()["outcome"],
        "unknown"
    );
    host.emit(
        id,
        json!({"type":"event","event":{"type":"turn/start","data":{"turn":2}}}),
    );
    host.emit(id, json!({"type":"event","event":{"type":"user/message","data":{"turn":2,"source":{"kind":"user","rpcId":"owned-input"},"content":[{"type":"text","text":"owned text"}]}}}));
    wait_native_input(&service, id, "owned text").await;
    assert_eq!(
        service.call("harness_operation", query).await.unwrap()["outcome"],
        "committed"
    );
    let page = events(&service).await;
    assert!(!page["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["message"]["method"] == "turn/completed"));
    host.emit(id, json!({"type":"event","event":{"type":"turn/end","data":{"turn":2,"reason":{"kind":"completed"}}}}));
    wait_terminal(&service, id).await;
    assert!(!host
        .state
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|row| row["method"] == "session/cancel"));
    service.close().await;
}

#[tokio::test]
async fn cold_history_proves_original_prompt_completion_without_adopting_the_session() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    for (kind, data) in [
        ("turn/start", json!({"turn":1})),
        (
            "user/message",
            json!({"source":{"kind":"user","rpcId":"original"},"content":[{"type":"text","text":"frozen input"}]}),
        ),
        (
            "assistant/message",
            json!({"content":[{"type":"text","text":"done"}]}),
        ),
        ("turn/end", json!({"turn":1,"reason":{"kind":"completed"}})),
    ] {
        host.emit(
            "existing",
            json!({"type":"event","event":{"type":kind,"data":data}}),
        );
    }
    let service = service(root.path(), &host.url);
    let history = service
        .call(
            "harness_session_read",
            json!({"profileId":"dsh","sessionId":"existing"}),
        )
        .await
        .unwrap();
    let turn = history["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|turn| turn["id"] == "existing-1")
        .unwrap();
    assert_eq!(turn["status"], "completed");
    assert_eq!(turn["submissionEvidence"]["requestId"], "original");
    use sha2::{Digest, Sha256};
    assert_eq!(
        turn["submissionEvidence"]["promptSha256"],
        format!("{:x}", Sha256::digest(b"frozen input"))
    );
    assert!(
        !host.state.calls.lock().unwrap().iter().any(|call| matches!(
            call["method"].as_str(),
            Some("session/prompt" | "session/create" | "session/resume")
        ))
    );
    service.close().await;
}
