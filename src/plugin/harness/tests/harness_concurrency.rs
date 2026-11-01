#[path = "support/events.rs"]
mod event_support;
use cells::{app::State, config::Config, projects::call};
use serde_json::{json, Value};
use std::{os::unix::fs::PermissionsExt, sync::Arc};

async fn fixture() -> (tempfile::TempDir, Arc<State>, Value) {
    let root = tempfile::tempdir().unwrap();
    for name in ["work", "home", "catalog"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    let peer = root.path().join("codex-fixture.py");
    std::fs::write(&peer, include_str!("fixtures/codex-app-server.py")).unwrap();
    std::fs::set_permissions(&peer, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config: Config = toml::from_str(&format!(
        "listen='127.0.0.1:0'\nexecution=true\nserver_id='parallel-test'\ntoken='isolated-concurrency-secret-0123456789'\n[projects]\nroot={:?}\n[[exports]]\nalias='work'\npath={:?}\naccess='rw'\n[[harnesses]]\nid='codex'\nkind='codex'\ncommand={:?}\nhome={:?}\nprojects=true\n",
        root.path().join("catalog"), root.path().join("work"), peer, root.path().join("home"))).unwrap();
    let state = State::from_config(&config).unwrap();
    cells::process::enable(&state).await.unwrap();
    let project = call(
        &state,
        0,
        "project_register",
        json!({"name":"Parallel", "alias":"work", "path":"", "access":"rw"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    let base = json!({"projectId":project["id"], "revision":project["revision"], "profileId":"codex", "epoch":state.services.require::<cells::harness::Harnesses>().unwrap().profiles()["epoch"]});
    (root, state, base)
}
async fn invoke(state: &Arc<State>, base: &Value, name: &str, id: &str, extra: Value) -> Value {
    let mut args = base.clone();
    args["requestId"] = json!(id);
    for (key, value) in extra.as_object().unwrap() {
        args[key] = value.clone();
    }
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .project_call(state, 0, name, args)
        .await
        .unwrap()
}
async fn create(state: &Arc<State>, base: &Value, id: &str) -> Value {
    let result = invoke(
        state,
        base,
        "harness_create",
        id,
        json!({"workspaceId":base["projectId"]}),
    )
    .await;
    assert_eq!(result["outcome"], "committed", "{result}");
    result["result"]["session"]["id"].clone()
}
async fn interrupt(state: &Arc<State>, base: &Value, id: &str, session: &Value, turn: &Value) {
    let result = invoke(
        state,
        base,
        "harness_interrupt",
        id,
        json!({"sessionId":session, "turnId":turn}),
    )
    .await;
    assert_eq!(result["outcome"], "committed", "{result}");
}

#[tokio::test]
async fn project_receipts_survive_noop_remount_and_revision_changes() {
    let (_root, state, base) = fixture().await;
    create(&state, &base, "stable-create").await;
    let receipt = invoke(
        &state,
        &base,
        "harness_operation",
        "stable-create",
        json!({}),
    )
    .await;
    for access in ["rw", "ro"] {
        call(
            &state,
            0,
            "project_remount",
            json!({"projectId":base["projectId"],"revision":base["revision"],"access":access}),
        )
        .await
        .unwrap();
        assert_eq!(
            invoke(
                &state,
                &base,
                "harness_operation",
                "stable-create",
                json!({})
            )
            .await,
            receipt
        );
    }
    let mut current = base.clone();
    current["revision"] = json!(2);
    assert_eq!(
        invoke(
            &state,
            &current,
            "harness_operation",
            "stable-create",
            json!({"readOnly":true})
        )
        .await,
        receipt
    );
    assert_eq!(
        state
            .services
            .require::<cells::harness::Harnesses>()
            .unwrap()
            .profiles()["epoch"],
        base["epoch"]
    );
    assert_eq!(
        invoke(
            &state,
            &base,
            "harness_create",
            "stable-create",
            json!({"workspaceId":base["projectId"]})
        )
        .await,
        receipt
    );
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
}

#[tokio::test]
async fn failed_configuration_preserves_receipts_and_never_repeats_a_native_create() {
    let (_root, state, base) = fixture().await;
    let first = invoke(
        &state,
        &base,
        "harness_create",
        "stable-create",
        json!({"workspaceId":base["projectId"]}),
    )
    .await;
    assert!(call(&state, 0, "project_configure", json!({"projectId":base["projectId"],"revision":base["revision"],"name":"Invalid","mounts":[{"alias":"missing","path":"","at":"/workspace/ref","access":"ro"}]})).await.is_err());
    assert_eq!(
        invoke(
            &state,
            &base,
            "harness_create",
            "stable-create",
            json!({"workspaceId":base["projectId"]})
        )
        .await,
        first
    );
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
}

#[tokio::test]
async fn project_turns_overlap_keep_independent_receipts_and_protect_files_until_the_last_completion(
) {
    let (_root, state, base) = fixture().await;
    let a = create(&state, &base, "create-a").await;
    let b = create(&state, &base, "create-b").await;
    let (first, second) = tokio::join!(
        invoke(
            &state,
            &base,
            "harness_turn",
            "turn-a",
            json!({"sessionId":a,"prompt":"a"})
        ),
        invoke(
            &state,
            &base,
            "harness_turn",
            "turn-b",
            json!({"sessionId":b,"prompt":"b"})
        )
    );
    for (id, receipt) in [("turn-a", &first), ("turn-b", &second)] {
        assert_eq!(receipt["outcome"], "committed", "{receipt}");
        assert_eq!(
            invoke(&state, &base, "harness_operation", id, json!({})).await,
            *receipt
        );
    }
    assert_ne!(first["result"]["turnId"], second["result"]["turnId"]);
    assert_eq!(
        invoke(
            &state,
            &base,
            "harness_turn",
            "duplicate-a",
            json!({"sessionId":a,"prompt":"duplicate"})
        )
        .await["code"],
        "EBUSY"
    );
    assert!(state.files.shared().is_err());
    assert!(state.files.exclusive().is_err());
    assert_eq!(
        call(
            &state,
            0,
            "project_configure",
            json!({"projectId":base["projectId"],"revision":1,"name":"Changed","mounts":[]})
        )
        .await
        .unwrap_err()
        .code,
        "EBUSY"
    );
    interrupt(&state, &base, "interrupt-a", &a, &first["result"]["turnId"]).await;
    assert!(state.files.shared().is_err());
    let b_state = invoke(
        &state,
        &base,
        "harness_session_read",
        "read-b",
        json!({"sessionId":b}),
    )
    .await;
    assert_eq!(b_state["session"]["status"], "active");
    interrupt(
        &state,
        &base,
        "interrupt-b",
        &b,
        &second["result"]["turnId"],
    )
    .await;
    assert!(state.files.shared().is_ok());
    assert!(state.files.exclusive().is_ok());
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
}

#[tokio::test]
async fn another_session_starts_while_the_first_is_waiting_for_approval_before_its_reply() {
    let (_root, state, base) = fixture().await;
    let a = create(&state, &base, "create-a").await;
    let b = create(&state, &base, "create-b").await;
    let (first_state, first_base, first_session) = (state.clone(), base.clone(), a.clone());
    let first = tokio::spawn(async move {
        invoke(
            &first_state,
            &first_base,
            "harness_turn",
            "turn-a",
            json!({"sessionId":first_session,"prompt":"approval-before-start-reply"}),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let events = event_support::scoped(
                state
                    .services
                    .require::<cells::harness::Harnesses>()
                    .unwrap(),
                &state,
                0,
                base.clone(),
            )
            .await
            .unwrap();
            if events["requests"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == "approval-early")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!first.is_finished());
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        invoke(
            &state,
            &base,
            "harness_turn",
            "turn-b",
            json!({"sessionId":b,"prompt":"b"}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(second["outcome"], "committed", "{second}");
    assert!(!first.is_finished());
    let approved = invoke(
        &state,
        &base,
        "harness_respond",
        "approve",
        json!({"nativeRequestId":"approval-early","response":{"decision":"accept"}}),
    )
    .await;
    assert_eq!(approved["outcome"], "committed");
    let first = first.await.unwrap();
    assert_eq!(first["outcome"], "committed");
    interrupt(&state, &base, "interrupt-a", &a, &first["result"]["turnId"]).await;
    assert!(state.files.shared().is_err());
    interrupt(
        &state,
        &base,
        "interrupt-b",
        &b,
        &second["result"]["turnId"],
    )
    .await;
    assert!(state.files.shared().is_ok());
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
}

#[tokio::test]
async fn a_terminal_event_before_the_start_reply_releases_the_correct_turn() {
    let (_root, state, base) = fixture().await;
    let a = create(&state, &base, "create-a").await;
    let result = invoke(
        &state,
        &base,
        "harness_turn",
        "early-completion",
        json!({"sessionId":a,"prompt":"complete-before-start-reply"}),
    )
    .await;
    assert_eq!(result["outcome"], "committed", "{result}");
    assert!(state.files.shared().is_ok());
    let next = invoke(
        &state,
        &base,
        "harness_turn",
        "next",
        json!({"sessionId":a,"prompt":"next"}),
    )
    .await;
    assert_eq!(next["outcome"], "committed");
    assert!(state.files.shared().is_err());
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
    assert!(state.files.shared().is_ok());
}

#[tokio::test]
async fn thirty_two_sessions_submit_while_another_waits_for_approval() {
    let (_root, state, base) = fixture().await;
    let blocked = create(&state, &base, "create-blocked").await;
    let (s, b, id) = (state.clone(), base.clone(), blocked.clone());
    let waiting = tokio::spawn(async move {
        invoke(
            &s,
            &b,
            "harness_turn",
            "blocked",
            json!({"sessionId":id,"prompt":"approval-before-start-reply"}),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let page = event_support::scoped(
                state
                    .services
                    .require::<cells::harness::Harnesses>()
                    .unwrap(),
                &state,
                0,
                base.clone(),
            )
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
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..32 {
        let (s, b) = (state.clone(), base.clone());
        tasks.spawn(async move {
            let id = create(&s, &b, &format!("create-{n}")).await;
            let turn = invoke(
                &s,
                &b,
                "harness_turn",
                &format!("turn-{n}"),
                json!({"sessionId":id,"prompt":"parallel"}),
            )
            .await;
            assert_eq!(turn["outcome"], "committed", "{turn}");
            let replay = invoke(&s, &b, "harness_operation", &format!("turn-{n}"), json!({})).await;
            assert_eq!(replay, turn);
            interrupt(
                &s,
                &b,
                &format!("interrupt-{n}"),
                &id,
                &turn["result"]["turnId"],
            )
            .await;
        });
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    })
    .await
    .expect("unrelated sessions must finish while approval is pending");
    assert!(!waiting.is_finished());
    let approved = invoke(
        &state,
        &base,
        "harness_respond",
        "approve",
        json!({"nativeRequestId":"approval-early","response":{"decision":"accept"}}),
    )
    .await;
    assert_eq!(approved["outcome"], "committed");
    let turn = waiting.await.unwrap();
    assert_eq!(turn["outcome"], "committed");
    interrupt(
        &state,
        &base,
        "interrupt-blocked",
        &blocked,
        &turn["result"]["turnId"],
    )
    .await;
    assert!(state.files.shared().is_ok());
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
}
