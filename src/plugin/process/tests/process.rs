use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use cells::{app::State, config::Config, router};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn send(app: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", "Bearer test-secret-at-least-24-bytes")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn terminal(app: &axum::Router, path: &str) -> Value {
    for _ in 0..500 {
        let (code, value) = send(app, "GET", path, Value::Null).await;
        assert_eq!(code, StatusCode::OK);
        if value["state"] != "running" {
            return value;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("Process did not terminate")
}

#[tokio::test]
async fn native_execution_confines_mounts_fences_revisions_and_cancels() {
    if std::env::var("CELLS_PROCESS_TEST").as_deref() != Ok("1") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let reference = root.path().join("reference");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&reference).unwrap();
    std::fs::write(project.join("note"), "before").unwrap();
    std::fs::write(reference.join("guide"), "reference").unwrap();
    let config: Config = toml::from_str(&format!(
        r#"
listen = "127.0.0.1:0"
server_id = "test-node"
execution = true
token = "test-secret-at-least-24-bytes"
[[exports]]
alias = "project"
path = "{}"
access = "rw"
[[exports]]
alias = "reference"
path = "{}"
"#,
        project.display(),
        reference.display()
    ))
    .unwrap();
    let state = State::from_config(&config).unwrap();
    cells::process::enable(&state).await.unwrap();
    let epoch = state
        .services
        .require::<cells::process::Execution>()
        .unwrap()
        .epoch()
        .to_owned();
    let app = router(state.clone(), &[]).unwrap();
    let request = json!({"serverId":"test-node", "epoch":epoch, "requestId":"one", "command":"/bin/bash", "args":["-c", "set -eu; test \"$(pwd)\" = /workspace; cat ../reference/guide; if echo bad > /reference/guide 2>/dev/null; then exit 7; fi; echo after > note"], "cwd":"/workspace", "timeoutMs":5000,
        "mounts":[{"alias":"project","path":"","at":"/workspace","access":"rw"},{"alias":"reference","path":"","at":"/reference","access":"ro"}]});
    let before = state
        .exports
        .get("project")
        .unwrap()
        .stat("note")
        .unwrap()
        .unwrap()
        .revision;
    assert_eq!(
        send(&app, "POST", "/v1/processes", request.clone()).await.0,
        StatusCode::OK
    );
    let path = format!("/v1/processes/{epoch}/one");
    let done = terminal(&app, &path).await;
    assert_eq!(done["state"], "exited", "{done}");
    assert_eq!(done["code"], 0, "{done}");
    assert_eq!(done["stdout"], "reference");
    assert_ne!(
        before,
        state
            .exports
            .get("project")
            .unwrap()
            .stat("note")
            .unwrap()
            .unwrap()
            .revision
    );
    let mut duplicate = request.clone();
    duplicate["args"] = json!(["-c", "echo replay > note"]);
    assert_eq!(
        send(&app, "POST", "/v1/processes", duplicate).await.1["state"],
        "exited"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("note")).unwrap(),
        "after\n"
    );
    let mut long = request.clone();
    long["requestId"] = json!("long");
    long["args"] = json!(["-c", "sleep 60"]);
    assert_eq!(
        send(&app, "POST", "/v1/processes", long).await.0,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/v1/fs/project/stat",
            json!({"paths":["note"]})
        )
        .await
        .1["code"],
        "EBUSY"
    );
    let path = format!("/v1/processes/{epoch}/long");
    send(&app, "POST", &(path.clone() + "/cancel"), Value::Null).await;
    assert_eq!(terminal(&app, &path).await["state"], "cancelled");
    let mut denied = request.clone();
    denied["requestId"] = json!("escape");
    denied["mounts"][0]["path"] = json!("../reference");
    assert_ne!(
        send(&app, "POST", "/v1/processes", denied).await.0,
        StatusCode::OK
    );
    // A nested mount point under a read-only parent cannot be created inside
    // the sandbox, so it is rejected before the launcher runs.
    let mut nested = request.clone();
    nested["requestId"] = json!("nested");
    nested["cwd"] = json!("/reference/inner");
    nested["mounts"] = json!([
        {"alias":"reference","path":"","at":"/reference","access":"ro"},
        {"alias":"project","path":"","at":"/reference/inner","access":"ro"}
    ]);
    assert_ne!(
        send(&app, "POST", "/v1/processes", nested).await.0,
        StatusCode::OK
    );
    let mut timed = request.clone();
    timed["requestId"] = json!("timeout");
    timed["timeoutMs"] = json!(50);
    timed["args"] = json!(["-c", "sleep 60"]);
    assert_eq!(
        send(&app, "POST", "/v1/processes", timed).await.0,
        StatusCode::OK
    );
    assert_eq!(
        terminal(&app, &format!("/v1/processes/{epoch}/timeout")).await["state"],
        "timed-out"
    );
    let mut background = request.clone();
    background["requestId"] = json!("background");
    background["args"] = json!([
        "-c",
        "(sleep 0.2; echo leaked > escaped) >/dev/null 2>&1 & echo done"
    ]);
    assert_eq!(
        send(&app, "POST", "/v1/processes", background).await.0,
        StatusCode::OK
    );
    assert_eq!(
        terminal(&app, &format!("/v1/processes/{epoch}/background")).await["state"],
        "exited"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!project.join("escaped").exists());
    let mut overflow = request.clone();
    overflow["requestId"] = json!("overflow");
    overflow["args"] = json!(["-c", "head -c 100000 /dev/zero"]);
    assert_eq!(
        send(&app, "POST", "/v1/processes", overflow).await.0,
        StatusCode::OK
    );
    assert_eq!(
        terminal(&app, &format!("/v1/processes/{epoch}/overflow")).await["truncated"],
        true
    );
    let mut stale = request;
    stale["epoch"] = json!("previous");
    assert_eq!(
        send(&app, "POST", "/v1/processes", stale).await.1["code"],
        "STALE_SERVER_EPOCH"
    );
}
