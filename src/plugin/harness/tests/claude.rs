#[path = "support/events.rs"]
mod event_support;
use nestos_node::{config::Config, harness::Harnesses};
use serde_json::{json, Value};

#[tokio::test]
async fn session_capacity_reclaims_idle_processes_but_keeps_history_receipts_and_pending_turns() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let question = mutate(
        &service,
        "harness_create",
        "capacity-question",
        json!({"workspaceId":"workspace"}),
    )
    .await["result"]["session"]["id"]
        .clone();
    mutate(
        &service,
        "harness_turn",
        "capacity-question-turn",
        json!({"sessionId":question,"prompt":"question"}),
    )
    .await;
    let active = mutate(
        &service,
        "harness_create",
        "capacity-active",
        json!({"workspaceId":"workspace"}),
    )
    .await["result"]["session"]["id"]
        .clone();
    mutate(
        &service,
        "harness_turn",
        "capacity-active-turn",
        json!({"sessionId":active,"prompt":"wait"}),
    )
    .await;
    let mut first = Value::Null;
    for index in 0..20 {
        let created = mutate(
            &service,
            "harness_create",
            &format!("capacity-create-{index}"),
            json!({"workspaceId":"workspace"}),
        )
        .await;
        assert_eq!(created["outcome"], "committed", "{created}");
        let id = created["result"]["session"]["id"].clone();
        if index == 0 {
            first = id.clone();
        }
        assert_eq!(
            mutate(
                &service,
                "harness_turn",
                &format!("capacity-turn-{index}"),
                json!({"sessionId":id,"prompt":"hello"})
            )
            .await["outcome"],
            "committed"
        );
        idle(&service, &id).await;
    }
    assert_eq!(
        call(&service, "harness_session_info", json!({"sessionId":first})).await["session"]
            ["owned"],
        false
    );
    assert!(
        call(&service, "harness_session_read", json!({"sessionId":first})).await["turns"]
            .to_string()
            .contains("Fixture reply")
    );
    assert_eq!(
        call(
            &service,
            "harness_operation",
            json!({"epoch":"epoch","requestId":"capacity-turn-0"})
        )
        .await["outcome"],
        "committed"
    );
    assert_eq!(
        call(
            &service,
            "harness_session_info",
            json!({"sessionId":question})
        )
        .await["session"]["owned"],
        true
    );
    assert_eq!(
        call(
            &service,
            "harness_session_info",
            json!({"sessionId":active})
        )
        .await["session"]["status"],
        "active"
    );
    assert_eq!(
        event_support::snapshot(&service, json!({"profileId":"claude"}))
            .await
            .unwrap()["requests"][0]["id"],
        "question-request"
    );
    assert_eq!(
        mutate(
            &service,
            "harness_resume",
            "capacity-resume",
            json!({"sessionId":first})
        )
        .await["outcome"],
        "committed"
    );
    assert_eq!(
        mutate(
            &service,
            "harness_turn",
            "capacity-resumed-turn",
            json!({"sessionId":first,"prompt":"hello again"})
        )
        .await["outcome"],
        "committed"
    );
    idle(&service, &first).await;
    service.close().await;
}

#[tokio::test]
async fn session_capacity_rejects_new_sessions_when_every_loaded_turn_is_active() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    for index in 0..16 {
        let created = mutate(
            &service,
            "harness_create",
            &format!("active-create-{index}"),
            json!({"workspaceId":"workspace"}),
        )
        .await;
        assert_eq!(created["outcome"], "committed");
        let id = created["result"]["session"]["id"].clone();
        assert_eq!(
            mutate(
                &service,
                "harness_turn",
                &format!("active-turn-{index}"),
                json!({"sessionId":id,"prompt":"wait"})
            )
            .await["outcome"],
            "committed"
        );
    }
    let rejected = mutate(
        &service,
        "harness_create",
        "active-overflow",
        json!({"workspaceId":"workspace"}),
    )
    .await;
    assert_eq!(rejected["outcome"], "not-committed");
    assert_eq!(rejected["code"], "CLAUDE_SESSION_LIMIT");
    service.close().await;
}

#[tokio::test]
async fn idle_reaping_keeps_history_and_receipts_and_never_unloads_a_pending_dialog() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let created = mutate(
        &service,
        "harness_create",
        "gc-create",
        json!({"workspaceId":"workspace"}),
    )
    .await;
    let id = created["result"]["session"]["id"].clone();
    mutate(
        &service,
        "harness_turn",
        "gc-turn",
        json!({"sessionId":id,"prompt":"hello"}),
    )
    .await;
    idle(&service, &id).await;
    service
        .reap_idle(std::time::Instant::now() + std::time::Duration::from_secs(301))
        .await;
    let cold = call(&service, "harness_session_info", json!({"sessionId":id})).await;
    assert_eq!(cold["session"]["owned"], false);
    assert_eq!(
        call(
            &service,
            "harness_operation",
            json!({"epoch":"epoch","requestId":"gc-turn"})
        )
        .await["outcome"],
        "committed"
    );
    assert!(
        !call(&service, "harness_session_read", json!({"sessionId":id})).await["turns"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        mutate(
            &service,
            "harness_resume",
            "gc-resume",
            json!({"sessionId":id})
        )
        .await["outcome"],
        "committed"
    );
    mutate(
        &service,
        "harness_turn",
        "gc-question",
        json!({"sessionId":id,"prompt":"question"}),
    )
    .await;
    for _ in 0..100 {
        if !event_support::snapshot(&service, json!({"profileId":"claude"}))
            .await
            .unwrap()["requests"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    service
        .reap_idle(std::time::Instant::now() + std::time::Duration::from_secs(3600))
        .await;
    let waiting = event_support::snapshot(&service, json!({"profileId":"claude"}))
        .await
        .unwrap();
    assert_eq!(waiting["requests"][0]["id"], "question-request");
    assert_eq!(
        call(&service, "harness_session_info", json!({"sessionId":id})).await["session"]["owned"],
        true
    );
    assert_eq!(mutate(&service, "harness_respond", "gc-answer", json!({"nativeRequestId":"question-request","response":{"answers":{"0":{"answers":["A"]}}}})).await["outcome"], "committed");
    idle(&service, &id).await;
    service.close().await;
}

#[tokio::test]
async fn native_models_configure_the_session_before_sending_input() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let catalog = call(&service, "harness_models", json!({})).await;
    assert_eq!(catalog["models"][0]["reasoningEfforts"][1]["id"], "high");
    assert_eq!(
        call(&service, "harness_sessions", json!({})).await["sessions"],
        json!([])
    );
    let created = mutate(
        &service,
        "harness_create",
        "model-create",
        json!({"workspaceId":"workspace"}),
    )
    .await;
    let id = &created["result"]["session"]["id"];
    let bad = mutate(
        &service,
        "harness_turn",
        "bad-model",
        json!({"sessionId":id,"prompt":"check-model-selection","selection":{"modelId":"unknown"}}),
    )
    .await;
    assert_eq!(bad["outcome"], "not-committed");
    let accepted = mutate(&service,"harness_turn","model-turn",json!({"sessionId":id,"prompt":"check-model-selection","selection":{"modelId":"fixture-model","reasoningEffort":"high"}})).await;
    assert_eq!(accepted["outcome"], "committed");
    let current = call(&service, "harness_models", json!({"sessionId":id})).await;
    assert_eq!(current["current"]["reasoningEffort"], "high");
    service.close().await;
}
use std::os::unix::fs::{symlink, PermissionsExt};
fn fixture(root: &std::path::Path) -> Harnesses {
    let script = root.join("claude-fixture.py");
    std::fs::write(&script, include_str!("fixtures/claude-sdk.py")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.join("native");
    std::fs::create_dir(&home).unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config:Config=toml::from_str(&format!("listen=\"127.0.0.1:0\"\nexecution=false\ntoken=\"claude-fixture-token-0123456789\"\n[[harnesses]]\nid=\"claude\"\nkind=\"claude\"\ncommand={script:?}\nhome={home:?}\n[[harnesses.workspaces]]\nid=\"workspace\"\npath={workspace:?}\n")).unwrap();
    Harnesses::new(
        nestos_node::harness::config::validate(&config).unwrap(),
        "epoch".into(),
    )
    .unwrap()
}
async fn call(service: &Harnesses, name: &str, extra: Value) -> Value {
    let mut args = json!({"profileId":"claude"});
    for (k, v) in extra.as_object().unwrap() {
        args[k] = v.clone();
    }
    service.call(name, args).await.unwrap()
}
async fn mutate(service: &Harnesses, name: &str, id: &str, extra: Value) -> Value {
    let mut args = extra;
    args["epoch"] = json!("epoch");
    args["requestId"] = json!(id);
    call(service, name, args).await
}
async fn idle(service: &Harnesses, id: &Value) {
    for _ in 0..100 {
        if call(service, "harness_session_info", json!({"sessionId":id})).await["session"]["status"]
            == "idle"
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("turn did not complete");
}

#[tokio::test]
async fn claude_status_queries_preserve_active_execution_and_native_system_output() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let created = mutate(
        &service,
        "harness_create",
        "state-create",
        json!({"workspaceId":"workspace"}),
    )
    .await;
    let id = created["result"]["session"]["id"].clone();
    let started = mutate(
        &service,
        "harness_turn",
        "state-turn",
        json!({"sessionId":id,"prompt":"wait"}),
    )
    .await;
    let turn = started["result"]["turnId"].clone();
    let status = mutate(
        &service,
        "harness_command",
        "state-query",
        json!({"sessionId":id,"command":"session:status"}),
    )
    .await;
    assert_eq!(status["outcome"], "committed");
    let state: Value = serde_json::from_str(status["result"]["output"].as_str().unwrap()).unwrap();
    assert_eq!(state["status"], "active");
    assert_eq!(state["activeTurnId"], turn);
    mutate(
        &service,
        "harness_interrupt",
        "state-stop",
        json!({"sessionId":id,"turnId":turn}),
    )
    .await;
    idle(&service, &id).await;
    mutate(
        &service,
        "harness_command",
        "compact-query",
        json!({"sessionId":id,"command":"compact"}),
    )
    .await;
    idle(&service, &id).await;
    let events = event_support::snapshot(&service, json!({"profileId":"claude"}))
        .await
        .unwrap();
    assert!(events["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["message"]["params"]["item"]["text"] == "Context compaction completed"));
    service.close().await;
}
#[tokio::test]
async fn native_command_catalog_and_dispatch_are_session_scoped_and_receipted() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let cold = call(&service, "harness_commands", json!({})).await;
    assert!(cold["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["name"] == "verify"));
    assert_eq!(
        call(&service, "harness_sessions", json!({})).await["sessions"],
        json!([])
    );
    let created = mutate(
        &service,
        "harness_create",
        "command-create",
        json!({"workspaceId":"workspace"}),
    )
    .await;
    let id = &created["result"]["session"]["id"];
    let query_args = json!({"profileId":"claude","sessionId":id,"command":"session:status","requestId":"read-state"});
    let observed = service
        .call("harness_query", query_args.clone())
        .await
        .unwrap();
    assert_eq!(observed["commandId"], "read-state");
    assert_eq!(observed["status"], "completed");
    let receipt = service
        .call(
            "harness_operation",
            json!({"profileId":"claude","epoch":"epoch","requestId":"read-state"}),
        )
        .await
        .unwrap();
    assert_eq!(receipt["outcome"], "not-committed");
    let mut serial = query_args;
    serial["command"] = json!("permissions");
    assert!(service.call("harness_query", serial).await.is_err());

    let catalog = call(&service, "harness_commands", json!({"sessionId":id})).await;
    let rows = catalog["commands"].as_array().unwrap();
    assert!(rows
        .iter()
        .any(|row| row["name"] == "team:verify" && row["kind"] == "prompt"));
    assert!(rows
        .iter()
        .any(|row| row["name"] == "verify" && row["kind"] == "prompt"));
    assert!(rows
        .iter()
        .any(|row| row["name"] == "compact" && row["kind"] == "action"));
    assert!(rows
        .iter()
        .any(|row| row["name"] == "condense" && row["kind"] == "action"));
    let args = json!({"sessionId":id,"command":"team:verify","arguments":"src"});
    let accepted = mutate(&service, "harness_command", "verify-once", args.clone()).await;
    assert_eq!(accepted["outcome"], "committed");
    assert_eq!(
        mutate(&service, "harness_command", "verify-once", args).await,
        accepted
    );
    idle(&service, id).await;
    let history = call(&service, "harness_session_read", json!({"sessionId":id})).await;
    assert!(history.to_string().contains("/team:verify src"));
    let alias = mutate(
        &service,
        "harness_command",
        "verify-alias",
        json!({"sessionId":id,"command":"verify","arguments":"tests"}),
    )
    .await;
    assert_eq!(alias["outcome"], "committed");
    idle(&service, id).await;
    assert!(
        call(&service, "harness_session_read", json!({"sessionId":id}))
            .await
            .to_string()
            .contains("/verify tests")
    );
    let compact = mutate(
        &service,
        "harness_command",
        "compact-alias",
        json!({"sessionId":id,"command":"condense"}),
    )
    .await;
    assert_eq!(compact["outcome"], "committed");
    assert!(compact["result"]["turnId"].is_string());
    idle(&service, id).await;
    assert!(
        !call(&service, "harness_session_read", json!({"sessionId":id}))
            .await
            .to_string()
            .contains("/condense")
    );
    assert_eq!(
        mutate(
            &service,
            "harness_command",
            "not-native",
            json!({"sessionId":id,"command":"quit"})
        )
        .await["outcome"],
        "not-committed"
    );
    service.close().await;
}

#[tokio::test]
async fn sdk_roundtrips_native_history_receipts_approvals_questions_and_interruption() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let descriptor = service.profiles();
    assert_eq!(descriptor["profiles"][0]["kind"], "claude");
    assert_eq!(descriptor["profiles"][0]["capabilities"]["archive"], false);
    let created = mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"workspace"}),
    )
    .await;
    assert_eq!(created["outcome"], "committed");
    let id = &created["result"]["session"]["id"];
    let resumed_empty = mutate(
        &service,
        "harness_resume",
        "resume-empty",
        json!({"sessionId":id}),
    )
    .await;
    assert_eq!(resumed_empty["outcome"], "committed");
    let args = json!({"sessionId":id,"prompt":"quoted \"text\"\n中文","attachments":[{"kind":"text","name":"notes.md","content":"fixture attachment"}]});
    let turn = mutate(&service, "harness_turn", "turn", args.clone()).await;
    assert_eq!(turn["outcome"], "committed");
    assert_eq!(mutate(&service, "harness_turn", "turn", args).await, turn);
    idle(&service, id).await;
    let history = call(
        &service,
        "harness_session_read",
        json!({"sessionId":id,"toolDetail":"summary"}),
    )
    .await;
    assert!(history.to_string().contains("quoted"));
    assert!(history.to_string().contains("Fixture reply"));
    let search = call(
        &service,
        "harness_session_search",
        json!({"query":"REPLY　fixture","mode":"content"}),
    )
    .await;
    assert_eq!(search["matches"].as_array().unwrap().len(), 1);
    for (query, count) in [
        ("中文 quoted", 1),
        ("fixture reply", 0),
        ("quoted absent", 0),
    ] {
        let prompts = call(
            &service,
            "harness_session_search",
            json!({"query":query,"mode":"prompt"}),
        )
        .await;
        assert_eq!(prompts["matches"].as_array().unwrap().len(), count);
    }

    assert_eq!(
        mutate(
            &service,
            "harness_turn",
            "tool",
            json!({"sessionId":id,"prompt":"tool-roundtrip"})
        )
        .await["outcome"],
        "committed"
    );
    idle(&service, id).await;
    let events = event_support::snapshot(&service, json!({"profileId":"claude"}))
        .await
        .unwrap();
    let tool_events = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["message"]["params"]["item"]["id"] == "fixture-tool")
        .collect::<Vec<_>>();
    assert_eq!(tool_events.len(), 2);
    assert_eq!(tool_events[0]["message"]["method"], "item/started");
    assert_eq!(
        tool_events[1]["message"]["params"]["item"]["status"],
        "completed"
    );
    assert!(!events.to_string().contains("private fixture tool output"));
    for (prompt, request, response) in [
        ("approval", "approval-request", json!({"decision":"accept"})),
        (
            "approval-cancel",
            "approval-request",
            json!({"decision":"cancel"}),
        ),
        (
            "question",
            "question-request",
            json!({"answers":{"0":{"answers":["A"]}}}),
        ),
    ] {
        let turn = mutate(
            &service,
            "harness_turn",
            prompt,
            json!({"sessionId":id,"prompt":prompt}),
        )
        .await;
        assert_eq!(turn["outcome"], "committed");
        for _ in 0..100 {
            if !event_support::snapshot(&service, json!({"profileId":"claude"}))
                .await
                .unwrap()["requests"]
                .as_array()
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            mutate(
                &service,
                "harness_respond",
                &format!("respond-{prompt}"),
                json!({"nativeRequestId":request,"response":response})
            )
            .await["outcome"],
            "committed"
        );
        idle(&service, id).await;
        if prompt == "approval-cancel" {
            assert_eq!(
                call(&service, "harness_session_info", json!({"sessionId":id})).await["session"]
                    ["lastTurnResult"],
                "cancelled"
            );
        }
    }
    let waiting = mutate(
        &service,
        "harness_turn",
        "wait",
        json!({"sessionId":id,"prompt":"wait"}),
    )
    .await;
    assert_eq!(
        mutate(
            &service,
            "harness_interrupt",
            "stop",
            json!({"sessionId":id,"turnId":waiting["result"]["turnId"]})
        )
        .await["outcome"],
        "committed"
    );
    idle(&service, id).await;
    assert_eq!(
        call(&service, "harness_session_info", json!({"sessionId":id})).await["session"]
            ["lastTurnResult"],
        "cancelled"
    );
    assert_eq!(
        mutate(
            &service,
            "harness_archive",
            "archive",
            json!({"sessionId":id})
        )
        .await["code"],
        "ECAPABILITY"
    );
    service.close().await;
}
#[tokio::test]
async fn catalog_checks_actual_cwd_and_never_follows_foreign_transcript_links() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let workspace = root.path().join("workspace");
    let key = workspace
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let directory = root.path().join("native/projects").join(key);
    std::fs::create_dir_all(&directory).unwrap();
    let id = "11111111-1111-4111-8111-111111111111";
    let record = json!({"type":"user","sessionId":id,"cwd":root.path(),"uuid":"user","message":{"content":"foreign"},"timestamp":"2026-10-09T03:00:00Z"});
    std::fs::write(directory.join(format!("{id}.jsonl")), format!("{record}\n")).unwrap();
    assert!(
        call(&service, "harness_sessions", json!({})).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let linked = "22222222-2222-4222-8222-222222222222";
    symlink(
        directory.join(format!("{id}.jsonl")),
        directory.join(format!("{linked}.jsonl")),
    )
    .unwrap();
    assert!(
        call(&service, "harness_sessions", json!({})).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    service.close().await;
}

#[tokio::test]
async fn native_pages_preserve_turn_identity_and_resume_uses_verified_transcript() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let cwd = root.path().join("workspace");
    let key = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let folder = root.path().join("native/projects").join(key);
    std::fs::create_dir_all(&folder).unwrap();
    let id = "33333333-3333-4333-8333-333333333333";
    let mut records = Vec::new();
    for turn in 0..3 {
        let user = format!("user-{turn}");
        records.push(json!({"type":"user","uuid":user,"sessionId":id,"cwd":cwd,"message":{"content":format!("Prompt {turn} 中文\nquoted \"text\"")}}));
        for index in 0..60 {
            records.push(json!({"type":"assistant","uuid":format!("record-{turn}-{index}"),"parentUuid":format!("other-{index}"),"sessionId":id,"cwd":cwd,"message":{"id":format!("message-{turn}-{index}"),"content":[{"type":"text","text":format!("Reply {turn}:{index}")}]}}));
        }
    }
    let path = folder.join(format!("{id}.jsonl"));
    std::fs::write(
        &path,
        records.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    let session = call(&service, "harness_session_info", json!({"sessionId":id})).await;
    assert_eq!(session["session"]["owned"], false);
    let before = std::fs::read(&path).unwrap();
    let commands = call(&service, "harness_commands", json!({"sessionId":id})).await;
    assert!(commands["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["name"] == "verify"));
    assert_eq!(
        call(&service, "harness_session_info", json!({"sessionId":id})).await["session"]["owned"],
        false
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let mut cursor = Value::Null;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let page = call(
            &service,
            "harness_session_read",
            json!({"sessionId":id,"cursor":cursor}),
        )
        .await;
        let mut count = 0;
        for envelope in page["turns"].as_array().unwrap() {
            for item in envelope["items"].as_array().unwrap() {
                count += 1;
                let identity = item["id"].as_str().unwrap();
                assert!(
                    seen.insert(identity.to_owned()),
                    "duplicate native item {identity}"
                );
                let turn = identity.split('-').nth(1).unwrap();
                assert_eq!(item["turnId"], format!("user-{turn}"));
            }
        }
        assert!(count <= 100);
        cursor = page["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    assert_eq!(seen.len(), 183);
    let resumed = mutate(
        &service,
        "harness_resume",
        "resume",
        json!({"sessionId":id}),
    )
    .await;
    assert_eq!(resumed["outcome"], "committed");
    assert_eq!(resumed["result"]["session"]["id"], id);
    assert_eq!(resumed["result"]["session"]["owned"], true);
    service.close().await;
}

#[tokio::test]
async fn rejects_foreign_records_inside_an_otherwise_authorized_transcript() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let cwd = root.path().join("workspace");
    let key = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let folder = root.path().join("native/projects").join(key);
    std::fs::create_dir_all(&folder).unwrap();
    let id = "44444444-4444-4444-8444-444444444444";
    let mut transcript = format!(
        "{}\n",
        json!({"type":"user","uuid":"user","sessionId":id,"cwd":cwd,"message":{"content":"Authorized prompt"}})
    );
    for index in 0..500 {
        let mut record = json!({"type":"assistant","uuid":format!("item-{index}"),"sessionId":id,"message":{"id":format!("message-{index}"),"content":[{"type":"text","text":"x".repeat(1024)}]}});
        if index == 250 {
            record["cwd"] = json!(root.path());
        }
        transcript.push_str(&format!("{record}\n"));
    }
    std::fs::write(folder.join(format!("{id}.jsonl")), transcript).unwrap();
    assert_eq!(
        call(&service, "harness_session_info", json!({"sessionId":id})).await["session"]["owned"],
        false
    );
    let page = service
        .call(
            "harness_session_read",
            json!({"profileId":"claude","sessionId":id}),
        )
        .await
        .unwrap();
    // The newest page fits; an older page must reject the foreign record.
    let error = service
        .call(
            "harness_session_read",
            json!({"profileId":"claude","sessionId":id,"cursor":page["nextCursor"]}),
        )
        .await;
    let error = if let Ok(page) = error {
        service
            .call(
                "harness_session_read",
                json!({"profileId":"claude","sessionId":id,"cursor":page["nextCursor"]}),
            )
            .await
            .unwrap_err()
    } else {
        error.unwrap_err()
    };
    assert_eq!(error.code, "CLAUDE_SESSION_MISMATCH");
    let resumed = mutate(
        &service,
        "harness_resume",
        "resume-foreign",
        json!({"sessionId":id}),
    )
    .await;
    assert_eq!(resumed["code"], "CLAUDE_SESSION_MISMATCH");
    assert_ne!(resumed["outcome"], "committed");
    service.close().await;
}

async fn admitted_receipt(service: &Harnesses, id: &str) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let value = call(
                service,
                "harness_operation",
                json!({"epoch":"epoch","requestId":id}),
            )
            .await;
            if value["outcome"] == "committed" {
                return value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Original receipt did not become committed")
}

async fn late_session(service: &Harnesses) -> Value {
    mutate(
        service,
        "harness_create",
        "late-create",
        json!({"workspaceId":"workspace"}),
    )
    .await["result"]["session"]["id"]
        .clone()
}
async fn interrupt_late(service: &Harnesses, id: &Value) -> Value {
    let state =
        call(service, "harness_session_info", json!({"sessionId":id})).await["session"].clone();
    assert_eq!(state["status"], "active");
    let control = mutate(
        service,
        "harness_interrupt",
        "late-stop",
        json!({"sessionId":id,"turnId":state["activeTurnId"]}),
    )
    .await;
    assert_eq!(control["outcome"], "committed");
    state["activeTurnId"].clone()
}
async fn assert_one_input(service: &Harnesses, id: &Value) {
    let history = call(service, "harness_session_read", json!({"sessionId":id})).await;
    let users = history["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|turn| turn["items"].as_array().unwrap())
        .filter(|item| item["type"] == "userMessage")
        .count();
    assert_eq!(users, 1);
}
#[tokio::test]
async fn late_native_admission_survives_the_reply_deadline_and_exact_interrupt() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let id = late_session(&service).await;
    let original = mutate(
        &service,
        "harness_turn",
        "late-turn",
        json!({"sessionId":id,"prompt":"late-admission"}),
    )
    .await;
    assert_eq!(original["outcome"], "unknown");
    let turn = interrupt_late(&service, &id).await;
    let receipt = admitted_receipt(&service, "late-turn").await;
    assert_eq!(receipt["result"]["turnId"], turn);
    idle(&service, &id).await;
    assert_one_input(&service, &id).await;
    let observed = call(
        &service,
        "harness_operation",
        json!({"epoch":"epoch","requestId":"late-turn"}),
    )
    .await;
    assert_eq!(observed, receipt);
    service.close().await;
}

async fn await_active(service: &Harnesses, id: &Value) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if call(service, "harness_session_info", json!({"sessionId":id})).await["session"]
                ["status"]
                == "active"
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Native turn did not start");
}
#[tokio::test]
async fn stopping_the_native_process_releases_an_unacknowledged_worker_without_accepting_it() {
    let root = tempfile::tempdir().unwrap();
    let service = std::sync::Arc::new(fixture(root.path()));
    let id = late_session(&service).await;
    let worker = service.clone();
    let input = id.clone();
    let work = tokio::spawn(async move {
        mutate(
            &worker,
            "harness_turn",
            "unacknowledged-turn",
            json!({"sessionId":input,"prompt":"late-admission"}),
        )
        .await
    });
    await_active(&service, &id).await;
    service.close().await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), work)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["outcome"], "unknown");
    assert!(result.get("result").is_none());
}

#[tokio::test]
async fn native_metrics_read_context_and_latest_native_estimate_without_summing_result_totals() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let created = mutate(
        &service,
        "harness_create",
        "metrics-create",
        json!({"workspaceId":"workspace"}),
    )
    .await;
    let id = created["result"]["session"]["id"].clone();
    mutate(
        &service,
        "harness_turn",
        "metrics-turn",
        json!({"sessionId":id,"prompt":"metrics-report"}),
    )
    .await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let value = service
                .call(
                    "harness_session_metrics",
                    json!({"profileId":"claude","sessionId":id}),
                )
                .await
                .unwrap();
            if value["metrics"]["cost"].is_object() {
                break value;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result["metrics"]["contextWindow"]["usedTokens"], 1234);
    assert_eq!(result["metrics"]["contextWindow"]["limitTokens"], 200000);
    assert_eq!(result["metrics"]["cost"]["amountUsd"], 0.123);
    assert_eq!(result["metrics"]["cost"]["kind"], "native-estimate");
    assert_eq!(
        result["metrics"]["quota"]["windows"][0]["usedPercent"],
        80.0
    );
    let again = service
        .call(
            "harness_session_metrics",
            json!({"profileId":"claude","sessionId":id}),
        )
        .await
        .unwrap();
    assert_eq!(again["metrics"]["cost"]["amountUsd"], 0.123);
    service.close().await;
}

#[tokio::test]
async fn streaming_steering_preserves_the_active_turn_and_does_not_approve_a_waiting_tool() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let created = mutate(
        &service,
        "harness_create",
        "steer-create",
        json!({"workspaceId":"workspace"}),
    )
    .await;
    let id = created["result"]["session"]["id"].clone();
    let started = mutate(
        &service,
        "harness_turn",
        "steer-start",
        json!({"sessionId":id,"prompt":"approval"}),
    )
    .await;
    let turn = started["result"]["turnId"].clone();
    let result = mutate(
        &service,
        "harness_steer",
        "steer-context",
        json!({"sessionId":id,"turnId":turn,"prompt":"Additional direction"}),
    )
    .await;
    assert_eq!(result["outcome"], "committed");
    assert_eq!(result["result"]["turnId"], turn);
    assert_eq!(result["result"]["targetTurnId"], turn);
    let state = call(&service, "harness_session_info", json!({"sessionId":id})).await;
    assert_eq!(state["session"]["activeTurnId"], turn);
    assert_eq!(state["session"]["status"], "active");
    let events = event_support::snapshot(&service, json!({"profileId":"claude"}))
        .await
        .unwrap();
    assert!(!events["requests"].as_array().unwrap().is_empty());
    let replay = mutate(
        &service,
        "harness_steer",
        "steer-context",
        json!({"sessionId":id,"turnId":turn,"prompt":"Additional direction"}),
    )
    .await;
    assert_eq!(replay, result);
    service.close().await;
}
#[tokio::test]
async fn streaming_steering_reports_a_native_next_turn_when_the_original_finishes_before_acknowledgement(
) {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let created = mutate(
        &service,
        "harness_create",
        "race-create",
        json!({"workspaceId":"workspace"}),
    )
    .await;
    let id = created["result"]["session"]["id"].clone();
    let started = mutate(
        &service,
        "harness_turn",
        "race-start",
        json!({"sessionId":id,"prompt":"wait"}),
    )
    .await;
    let turn = started["result"]["turnId"].clone();
    let result = mutate(
        &service,
        "harness_steer",
        "race-context",
        json!({"sessionId":id,"turnId":turn,"prompt":"steer-after-result"}),
    )
    .await;
    assert_eq!(result["outcome"], "committed");
    assert_eq!(result["result"]["targetTurnId"], turn);
    assert_ne!(result["result"]["turnId"], turn);
    assert!(result["result"]["turnId"].is_string());
    let receipt = call(
        &service,
        "harness_operation",
        json!({"epoch":"epoch","requestId":"race-context"}),
    )
    .await;
    assert_eq!(receipt, result);
    service.close().await;
}
