#[allow(dead_code)]
#[path = "../../common/tests/support/directory_acl.rs"]
mod support;
use cells::{app::State, projects};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

fn transcript(home: &Path, cwd: &str, id: &str, foreign: Option<&Path>) {
    let key = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let directory = home.join("projects").join(key);
    std::fs::create_dir_all(&directory).unwrap();
    let record = json!({"type":"user","uuid":"prompt","sessionId":id,
        "cwd":foreign.map_or_else(|| cwd.to_owned(), |p| p.display().to_string()),
        "message":{"content":"Project history fixture"}});
    std::fs::write(directory.join(format!("{id}.jsonl")), format!("{record}\n")).unwrap();
}

fn configured() -> (tempfile::TempDir, Arc<State>) {
    let (root, previous) = support::fixture();
    drop(previous);
    let home = root.path().join("native");
    std::fs::create_dir(&home).unwrap();
    let template = include_str!("../../../../profiles/claude.example.toml")
        .replace("/home/USER/.claude", home.to_str().unwrap())
        .replace("principal_id = \"owner\"", "principal_id = \"alice\"");
    let parsed: cells::config::Config =
        toml::from_str(&format!("listen='127.0.0.1:0'\n{template}")).unwrap();
    let mut config = support::config(root.path(), false);
    config.harnesses = parsed.harnesses;
    let state = State::from_config(&config).unwrap();
    (root, state)
}

fn history(root: &Path, project: &Value) {
    let home = root.join("native");
    let physical = root.join("docs");
    let virtual_cwd = format!("/projects/{}", project["id"].as_str().unwrap());
    transcript(
        &home,
        physical.to_str().unwrap(),
        "11111111-1111-4111-8111-111111111111",
        None,
    );
    transcript(
        &home,
        &virtual_cwd,
        "22222222-2222-4222-8222-222222222222",
        None,
    );
    transcript(
        &home,
        physical.to_str().unwrap(),
        "33333333-3333-4333-8333-333333333333",
        Some(root),
    );
}

async fn verify_history(state: &Arc<State>, alice: usize, args: &Value) {
    let listed = state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .project_call(state, alice, "harness_sessions", args.clone())
        .await
        .unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 2);
    for session in listed["sessions"].as_array().unwrap() {
        assert_eq!(session["status"], "notLoaded");
        assert_eq!(session["owned"], false);
        let mut args = args.clone();
        args["sessionId"] = session["id"].clone();
        let read = state
            .services
            .require::<cells::harness::Harnesses>()
            .unwrap()
            .project_call(state, alice, "harness_session_read", args)
            .await
            .unwrap();
        assert_eq!(read["turns"][0]["items"][0]["type"], "userMessage");
        assert_eq!(read["session"]["owned"], false);
    }
}

#[tokio::test]
async fn claude_template_reads_native_and_virtual_project_history_without_adopting_sessions() {
    let (root, state) = configured();
    let alice = state.auth.index("alice").unwrap();
    let project = projects::call(
        &state,
        alice,
        "project_register",
        json!({"name":"Claude","alias":"docs","path":"","access":"ro"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    history(root.path(), &project);
    let args =
        json!({"profileId":"claude","projectId":project["id"],"revision":project["revision"]});
    verify_history(&state, alice, &args).await;
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
}
