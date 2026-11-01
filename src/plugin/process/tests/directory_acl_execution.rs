use axum::http::StatusCode;
use cells::{app::State, projects};
use serde_json::json;
#[allow(dead_code)]
#[path = "../../common/tests/support/directory_acl.rs"]
mod support;
use support::*;
#[tokio::test]
async fn explicit_execution_grant_allows_a_readonly_native_session_without_file_write_access() {
    use std::os::unix::fs::PermissionsExt;
    let (root, state) = fixture();
    drop(state);
    let home = root.path().join("alice-home");
    std::fs::create_dir(&home).unwrap();
    let peer = root.path().join("codex-fixture.py");
    std::fs::write(
        &peer,
        include_str!("../../harness/tests/fixtures/codex-app-server.py"),
    )
    .unwrap();
    std::fs::set_permissions(&peer, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut c = config(root.path(), false);
    c.execution = true;
    c.exports[0].grants[0].access = cells::config::Access::Ro;
    c.exports[0].grants[0].execute = true;
    c.harnesses=vec![toml::from_str(&format!("id='model'\nkind='codex'\nprincipal_id='alice'\nprojects=true\ncommand={peer:?}\nhome={home:?}")).unwrap()];
    let state = State::from_config(&c).unwrap();
    cells::process::enable(&state).await.unwrap();
    let who = state.auth.index("alice").unwrap();
    let project = projects::call(
        &state,
        who,
        "project_register",
        json!({"name":"Analysis","alias":"docs","path":"","access":"ro"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    assert_eq!(project["capabilities"]["fileWrite"], false);
    assert_eq!(project["capabilities"]["sessionControl"], true);
    let mut args = json!({"profileId":"model","projectId":project["id"],"revision":project["revision"],"epoch":state.services.require::<cells::harness::Harnesses>().unwrap().profiles()["epoch"],"requestId":"create","workspaceId":project["id"]});
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
    args["prompt"] = json!("read-only analysis");
    let turn = state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .project_call(&state, who, "harness_turn", args.clone())
        .await
        .unwrap();
    assert_eq!(turn["outcome"], "committed", "{turn}");
    args["requestId"] = json!("interrupt");
    args["turnId"] = turn["result"]["turnId"].clone();
    assert_eq!(
        state
            .services
            .require::<cells::harness::Harnesses>()
            .unwrap()
            .project_call(&state, who, "harness_interrupt", args)
            .await
            .unwrap()["outcome"],
        "committed"
    );
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
    assert_eq!(
        request(
            &state,
            Some(ALICE),
            "PUT",
            "/v1/fs/docs/content?path=new.txt",
            json!("new")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn execution_permission_does_not_make_a_readonly_directory_writable_in_the_os_sandbox() {
    let (root, state) = fixture();
    drop(state);
    let mut c = config(root.path(), false);
    c.execution = true;
    c.exports[0].grants[0].access = cells::config::Access::Ro;
    c.exports[0].grants[0].execute = true;
    let state = State::from_config(&c).unwrap();
    cells::process::enable(&state).await.unwrap();
    let who = state.auth.index("alice").unwrap();
    let p = projects::call(
        &state,
        who,
        "project_register",
        json!({"name":"RO","alias":"docs","path":"","access":"ro"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    projects::call(&state,who,"project_exec",json!({"projectId":p["id"],"revision":p["revision"],"requestId":"ro-shell","epoch":state.services.require::<cells::process::Execution>().unwrap().epoch(),"command":"/bin/sh","args":["-c","cat /workspace/readme.txt; if touch /workspace/not-allowed; then exit 9; fi"],"timeoutMs":5000})).await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(7), async {
        loop {
            let status = cells::process::status(
                &state,
                who,
                state
                    .services
                    .require::<cells::process::Execution>()
                    .unwrap()
                    .epoch(),
                "ro-shell",
            )
            .unwrap();
            if status.state != "running" {
                break status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        result.code,
        Some(0),
        "{} {} {:?}",
        result.stdout,
        result.stderr,
        result.error
    );
    assert_eq!(result.stdout, "public fixture");
    assert!(!root.path().join("docs/not-allowed").exists());
}
