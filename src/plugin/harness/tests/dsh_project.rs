use futures_util::StreamExt;
#[allow(dead_code)]
#[path = "support/dsh.rs"]
mod native;
#[allow(dead_code)]
#[path = "../../common/tests/support/directory_acl.rs"]
mod support;
use cells::{app::State, projects};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn managed_dsh_uses_the_project_launcher_and_readonly_mounts() {
    let directory = tempfile::Builder::new()
        .prefix("cells-dsh-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let (root, state) = support::fixture_at(directory);
    drop(state);
    let home = root.path().join("alice-dsh");
    std::fs::create_dir(&home).unwrap();
    let command = root.path().join("dsh-peer");
    std::fs::write(&command, "#!/bin/sh\nexit 3\n").unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = support::config(root.path(), false);
    config.execution = true;
    config.projects.as_mut().unwrap().network = true;
    config.exports[0].grants[0].access = cells::config::Access::Ro;
    config.exports[0].grants[0].execute = true;
    config.harnesses=vec![toml::from_str(&format!("id='dsh'\nkind='dsh'\nprincipal_id='alice'\nprojects=true\ncommand={command:?}\nhome={home:?}")).unwrap()];
    config.harnesses[0].aliases = vec!["old-dsh".into()];
    let state = State::from_config(&config).unwrap();
    cells::process::enable(&state).await.unwrap();
    let who = state.auth.index("alice").unwrap();
    let project = projects::call(
        &state,
        who,
        "project_register",
        json!({"name":"DSH","alias":"docs","path":"","access":"ro"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    // Existing native sessions retain their original physical workspace path.
    let host = native::Host::new(&root.path().join("docs")).await;
    std::fs::write(&command,format!("#!/bin/sh\n[ -f readme.txt ] || exit 8\nif touch forbidden-write 2>/dev/null; then exit 9; fi\n[ -f {source:?}/readme.txt ] || exit 10\nif touch {source:?}/native-forbidden-write 2>/dev/null; then exit 11; fi\n[ ! -e {private:?}/secret.txt ] || exit 12\nprintf 'dsh web: {}?token=fixture\\n'\nexec sleep 300\n",host.url,source=root.path().join("docs"),private=root.path().join("private"))).unwrap();
    let mut args = json!({"profileId":"old-dsh","projectId":project["id"],"revision":project["revision"],"epoch":state.services.require::<cells::harness::Harnesses>().unwrap().profiles()["epoch"],"requestId":"create","workspaceId":project["id"]});
    let listed = state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .project_call(&state, who, "harness_sessions", args.clone())
        .await
        .unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["id"], "existing");
    let mut query_args = args.clone();
    query_args.as_object_mut().unwrap().remove("epoch");
    query_args["sessionId"] = json!("existing");
    query_args["command"] = json!("session:status");
    query_args["requestId"] = json!("readonly-query");
    query_args["readOnly"] = json!(true);
    let observed = state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .project_call(&state, who, "harness_query", query_args.clone())
        .await
        .unwrap();
    assert_eq!(observed["status"], "completed");
    query_args["command"] = json!("compact");
    assert!(state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .project_call(&state, who, "harness_query", query_args)
        .await
        .is_err());
    let created = state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .project_call(&state, who, "harness_create", args.clone())
        .await
        .unwrap();
    assert_eq!(created["outcome"], "committed", "{created}");
    args["sessionId"] = created["result"]["session"]["id"].clone();
    args["requestId"] = json!("turn");
    args["prompt"] = json!("read only");
    let turn = state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .project_call(&state, who, "harness_turn", args.clone())
        .await
        .unwrap();
    assert_eq!(turn["outcome"], "committed", "{turn}");
    let id = args["sessionId"].as_str().unwrap();
    host.emit(
        id,
        json!({"type":"event","event":{"seq":0,"type":"turn/start","data":{"turn":1}}}),
    );
    host.emit(id,json!({"type":"event","event":{"seq":1,"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}));
    for _ in 0..40 {
        let page = state
            .services
            .require::<cells::harness::Harnesses>()
            .unwrap()
            .subscribe(&state, who, args.clone())
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap();
        if page["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["message"]["method"] == "turn/completed")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let harness = state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap();
    host.state.errors.lock().unwrap().insert(
        "session/prompt".into(),
        json!({"code":"gateway/internal","message":"private"}),
    );
    let proof_id = id.to_owned();
    args["requestId"] = json!("project-proof");
    args["prompt"] = json!("identified project input");
    assert_eq!(
        harness
            .project_call(&state, who, "harness_turn", args.clone())
            .await
            .unwrap()["outcome"],
        "unknown"
    );
    host.emit(
        &proof_id,
        json!({"type":"event","event":{"seq":2,"type":"turn/start","data":{"turn":2}}}),
    );
    host.emit(&proof_id, json!({"type":"event","event":{"seq":3,"type":"user/message","data":{"turn":2,"source":{"kind":"user","rpcId":"project-proof"},"content":[{"type":"text","text":"identified project input"}]}}}));
    host.emit(&proof_id, json!({"type":"event","event":{"seq":4,"type":"turn/end","data":{"turn":2,"reason":{"kind":"completed"}}}}));
    for _ in 0..100 {
        let page = harness
            .subscribe(&state, who, args.clone())
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap();
        if page["events"].as_array().unwrap().iter().any(|row| {
            row["message"]["method"] == "turn/completed"
                && row["message"]["params"]["turn"]["id"] == "project-proof"
        }) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Receipt identity spans observation modes; querying must not initialize another driver.
    args["readOnly"] = json!(true);
    assert_eq!(
        harness
            .project_call(&state, who, "harness_operation", args.clone())
            .await
            .unwrap()["outcome"],
        "committed"
    );
    assert_eq!(
        host.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row["method"] == "session/create")
            .count(),
        1
    );
    assert!(!root.path().join("docs/forbidden-write").exists());
    assert!(!root.path().join("docs/native-forbidden-write").exists());
    let bob = state.auth.index("bob").unwrap();
    assert!(state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .project_call(&state, bob, "harness_sessions", args)
        .await
        .is_err());
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
    assert_eq!(
        support::request(
            &state,
            Some(support::ALICE),
            "PUT",
            "/v1/fs/docs/content?path=new",
            Value::Null
        )
        .await
        .0,
        axum::http::StatusCode::FORBIDDEN
    );
}
