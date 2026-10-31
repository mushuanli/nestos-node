use nestos_node::{
    app::State,
    config::Config,
    projects::{call, runtime::ProjectRuntime},
};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

fn deployment(root: &Path) -> Config {
    for name in ["exports", "reference", "catalog", "codex"] {
        std::fs::create_dir_all(root.join(name)).unwrap();
    }
    std::fs::create_dir_all(root.join("exports/group/existing/ref")).unwrap();
    toml::from_str(&format!(
        r#"
listen="127.0.0.1:0"
execution=true
server_id="test-node"
token="project-test-secret-0123456789"
[projects]
root={catalog:?}
[[exports]]
alias="home"
path={exports:?}
access="rw"
[[exports]]
alias="reference"
path={reference:?}
[[harnesses]]
id="codex"
kind="codex"
home={home:?}
projects=true
"#,
        catalog = root.join("catalog"),
        exports = root.join("exports"),
        reference = root.join("reference"),
        home = root.join("codex")
    ))
    .unwrap()
}
async fn register(state: &Arc<State>, path: &str, create: bool) -> Value {
    call(
        state,
        0,
        "project_register",
        json!({"name":"Notes","alias":"home","path":path,"access":"rw","createDirectory":create}),
    )
    .await
    .unwrap()["project"]
        .clone()
}

#[tokio::test]
async fn harness_home_executable_can_read_sibling_resources_without_extra_mounts() {
    if std::env::var("PI_AGENT_PROJECT_TEST").as_deref() != Ok("1") {return;}
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let config = deployment(root.path());
    let state = State::from_config(&config).unwrap();
    let project = register(&state, "group/existing", false).await;
    state.services.require::<nestos_node::process::Execution>().unwrap().accept();
    let stored = state.services.require::<nestos_node::projects::ProjectService>().unwrap()
        .get(0, project["id"].as_str().unwrap()).unwrap();
    let runtime = ProjectRuntime::new(&state, 0, stored);
    let mut profile = nestos_node::harness::config::validate(&config).unwrap().remove(0);
    let binary = profile.home.join("native");
    std::fs::write(&binary, "#!/bin/sh\nexec /bin/cat \"$(dirname \"$0\")/resource\"\n").unwrap();
    std::fs::write(profile.home.join("resource"), "ready").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    profile.command = binary.to_string_lossy().into_owned();
    let mut prepared = runtime.prepare(&profile, vec![]).unwrap();
    let result = prepared.command.output().await.unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(result.stdout, b"ready");
}

#[tokio::test]
async fn source_aliases_refuse_runtime_paths_without_changing_default_namespaces() {
    let root = tempfile::tempdir().unwrap();
    let config = deployment(root.path());
    let state = State::from_config(&config).unwrap();
    let project = register(&state, "group/existing", false).await;
    let stored = state
        .services
        .get::<nestos_node::projects::ProjectService>()
        .unwrap()
        .get(0, project["id"].as_str().unwrap())
        .unwrap();
    let runtime = ProjectRuntime::new(&state, 0, stored);
    state
        .services
        .require::<nestos_node::process::Execution>()
        .unwrap()
        .accept();
    let mut profile = nestos_node::harness::config::validate(&config)
        .unwrap()
        .remove(0);
    profile.command = "/bin/true".into();
    match runtime.prepare_with_source_paths(&profile, vec![]) {
        Ok(_) => panic!("Source aliases must not shadow /tmp"),
        Err(error) => assert_eq!(error.code, "HARNESS_SOURCE_PATH_UNAVAILABLE"),
    }
    assert!(runtime.prepare(&profile, vec![]).is_ok());
}
async fn remount(
    state: &Arc<State>,
    project: &Value,
    access: &str,
) -> Result<Value, nestos_node::core::error::Error> {
    call(
        state,
        0,
        "project_remount",
        json!({"projectId":project["id"], "revision":project["revision"], "access":access}),
    )
    .await
}

#[tokio::test]
async fn remount_preserves_identity_and_files_and_fences_stale_revisions() {
    let root = tempfile::tempdir().unwrap();
    let state = State::from_config(&deployment(root.path())).unwrap();
    let original = register(&state, "group/existing", false).await;
    let file = root.path().join("exports/group/existing/note.txt");
    std::fs::write(&file, "preserved").unwrap();
    let readonly = remount(&state, &original, "ro").await.unwrap()["project"].clone();
    assert_eq!(readonly["id"], original["id"]);
    assert_eq!(readonly["access"], "ro");
    assert_eq!(readonly["revision"], 2);
    assert_eq!(
        remount(&state, &original, "rw").await.unwrap_err().code,
        "PROJECT_REVISION_CHANGED"
    );
    let writable = remount(&state, &readonly, "rw").await.unwrap()["project"].clone();
    assert_eq!(writable["id"], original["id"]);
    assert_eq!(writable["access"], "rw");
    assert_eq!(writable["revision"], 3);
    assert_eq!(std::fs::read_to_string(file).unwrap(), "preserved");
    let _guard = state.files.exclusive().unwrap();
    assert_eq!(
        remount(&state, &writable, "ro").await.unwrap_err().code,
        "EBUSY"
    );
}

#[tokio::test]
async fn remount_cannot_escalate_a_readonly_export() {
    let root = tempfile::tempdir().unwrap();
    let state = State::from_config(&deployment(root.path())).unwrap();
    let project = call(
        &state,
        0,
        "project_register",
        json!({"name":"Reference", "alias":"reference", "path":"", "access":"ro"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    assert!(remount(&state, &project, "rw").await.is_err());
    let stored = call(
        &state,
        0,
        "project_read",
        json!({"projectId":project["id"]}),
    )
    .await
    .unwrap();
    assert_eq!(stored["project"]["access"], "ro");
    assert_eq!(stored["project"]["revision"], 1);
}
#[tokio::test]
async fn registers_subdirectories_creates_one_child_deduplicates_and_survives_restart() {
    let root = tempfile::tempdir().unwrap();
    let config = deployment(root.path());
    let state = State::from_config(&config).unwrap();
    let first = register(&state, "group//existing/", false).await;
    assert_eq!(first["path"], "group/existing");
    assert_eq!(first, register(&state, "group/existing", false).await);
    let created = register(&state, "group/new", true).await;
    assert!(root.path().join("exports/group/new").is_dir());
    assert_ne!(first["id"], created["id"]);
    for path in ["../escape", "/etc", "group/no-parent/child"] {
        assert!(call(
            &state,
            0,
            "project_register",
            json!({"name":"X","alias":"home","path":path,"access":"rw","createDirectory":true})
        )
        .await
        .is_err());
    }
    drop(state);
    let state = State::from_config(&config).unwrap();
    assert_eq!(first, register(&state, "group/existing", false).await);
    assert_eq!(
        call(&state, 0, "project_list", json!({})).await.unwrap()["projects"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
#[tokio::test]
async fn mounts_are_server_owned_attenuated_and_revision_fenced() {
    let root = tempfile::tempdir().unwrap();
    let state = State::from_config(&deployment(root.path())).unwrap();
    let project = register(&state, "group/existing", false).await;
    let id = project["id"].as_str().unwrap();
    let configured=call(&state,0,"project_configure",json!({"projectId":id,"revision":1,"name":"Notes","mounts":[{"alias":"reference","path":"","at":"/workspace/ref","access":"ro"}]})).await.unwrap()["project"].clone();
    assert_eq!(configured["revision"], 2);
    assert!(call(
        &state,
        0,
        "project_configure",
        json!({"projectId":id,"revision":1,"name":"Notes","mounts":[]})
    )
    .await
    .is_err());
    for (alias, at, access) in [
        ("unknown", "/workspace/ref", "ro"),
        ("reference", "/etc", "ro"),
        ("reference", "/workspace/ref", "rw"),
        ("home", "/workspace/ref", "ro"),
    ] {
        assert!(call(&state,0,"project_configure",json!({"projectId":id,"revision":2,"name":"X","mounts":[{"alias":alias,"path":"","at":at,"access":access}]})).await.is_err());
    }
    let guard = state.files.exclusive().unwrap();
    assert_eq!(
        call(
            &state,
            0,
            "project_configure",
            json!({"projectId":id,"revision":2,"name":"X","mounts":[]})
        )
        .await
        .unwrap_err()
        .code,
        "EBUSY"
    );
    drop(guard);
    assert_eq!(call(&state,0,"project_exec",json!({"projectId":id,"revision":1,"epoch":state.services.require::<nestos_node::process::Execution>().unwrap().epoch(),"requestId":"cmd","command":"/bin/true","args":[]})).await.unwrap_err().code,"PROJECT_REVISION_CHANGED");
}
#[tokio::test]
async fn replacement_does_not_silently_retarget_projects_and_launcher_contains_only_grants() {
    let root = tempfile::tempdir().unwrap();
    let config = deployment(root.path());
    let state = State::from_config(&config).unwrap();
    let project = register(&state, "group/existing", false).await;
    state
        .services
        .require::<nestos_node::process::Execution>()
        .unwrap()
        .accept();
    let stored = state
        .services
        .get::<nestos_node::projects::ProjectService>()
        .unwrap()
        .get(0, project["id"].as_str().unwrap())
        .unwrap();
    let runtime = ProjectRuntime::new(&state, 0, stored);
    let fixture = root.path().join("codex-native");
    std::fs::write(&fixture, "executable").unwrap();
    let mut profile = nestos_node::harness::config::validate(&config)
        .unwrap()
        .remove(0);
    profile.command = fixture.to_str().unwrap().into();
    assert_eq!(
        runtime
            .resolve_cwd(root.path().join("exports/group/existing").to_str().unwrap())
            .unwrap(),
        runtime.cwd()
    );
    assert!(runtime
        .resolve_cwd(root.path().join("reference").to_str().unwrap())
        .is_err());
    let prepared = runtime
        .prepare(&profile, vec!["app-server".into(), "--stdio".into()])
        .unwrap();
    let args = prepared
        .command
        .as_std()
        .get_args()
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(args.contains(&runtime.cwd()));
    assert!(args.contains(&"/harness".into()));
    assert!(args.contains(&fixture.to_string_lossy().into_owned()));
    assert!(!args.contains(&root.path().to_string_lossy().into_owned()));
    assert!(!args.contains(&"--share-net".into()));
    runtime.begin("thread", "operation").unwrap();
    runtime.bind_turn("thread", "operation", "turn");
    assert!(state.files.shared().is_err());
    runtime.event(
        &json!({"method":"turn/completed","params":{"threadId":"other","turn":{"id":"turn"}}}),
    );
    assert!(state.files.shared().is_err());
    runtime.event(
        &json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"turn"}}}),
    );
    assert!(state.files.shared().is_ok());
    drop(prepared);
    std::fs::rename(
        root.path().join("exports/group/existing"),
        root.path().join("exports/group/old"),
    )
    .unwrap();
    std::fs::create_dir(root.path().join("exports/group/existing")).unwrap();
    assert_eq!(register_error(&state).await, "PROJECT_DIRECTORY_CHANGED");
    let forgotten = call(
        &state,
        0,
        "project_forget",
        json!({"projectId":project["id"],"revision":1}),
    )
    .await
    .unwrap();
    assert_eq!(forgotten["forgotten"], project["id"]);
    let replacement = register(&state, "group/existing", false).await;
    assert_ne!(replacement["id"], project["id"]);
    assert!(root.path().join("exports/group/old").exists());
}
async fn register_error(state: &Arc<State>) -> &'static str {
    call(
        state,
        0,
        "project_register",
        json!({"name":"Notes","alias":"home","path":"group/existing","access":"rw"}),
    )
    .await
    .unwrap_err()
    .code
}

#[tokio::test]
async fn native_project_bash_and_codex_share_directory_grants() {
    if std::env::var("PI_AGENT_PROJECT_TEST").as_deref() != Ok("1") {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let mut config = deployment(root.path());
    config.execution = true;
    let fixture = root.path().join("codex-fixture.py");
    std::fs::write(
        &fixture,
        include_str!("../../harness/tests/fixtures/codex-app-server.py"),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    config.harnesses[0].command = if std::env::var("PI_AGENT_PROJECT_CODEX").as_deref() == Ok("1") {
        "codex".into()
    } else {
        fixture.to_str().unwrap().into()
    };
    let state = State::from_config(&config).unwrap();
    nestos_node::process::enable(&state).await.unwrap();
    let project = register(&state, "group/existing", false).await;
    let id = project["id"].as_str().unwrap();
    let configured=call(&state,0,"project_configure",json!({"projectId":id,"revision":1,"name":"Notes","mounts":[{"alias":"reference","path":"","at":"/workspace/ref","access":"ro"}]})).await.unwrap()["project"].clone();
    std::fs::write(root.path().join("reference/guide"), "reference").unwrap();
    let started=call(&state,0,"project_exec",json!({"projectId":id,"revision":2,"epoch":state.services.require::<nestos_node::process::Execution>().unwrap().epoch(),"requestId":"bash","command":"/bin/bash","args":["-c","set -eu; test $(pwd) = /workspace; cat ref/guide; test ! -e /etc/passwd; if echo x > ref/guide 2>/dev/null; then exit 7; fi; echo project > note"],"timeoutMs":5000})).await.unwrap();
    assert!(started["process"].is_object());
    let result = loop {
        let result = nestos_node::process::status(
            &state,
            0,
            state
                .services
                .require::<nestos_node::process::Execution>()
                .unwrap()
                .epoch(),
            "bash",
        )
        .unwrap();
        if result.state != "running" {
            break result;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(result.code, Some(0));
    assert_eq!(
        std::fs::read_to_string(root.path().join("exports/group/existing/note")).unwrap(),
        "project\n"
    );
    let epoch = state
        .services
        .require::<nestos_node::harness::Harnesses>()
        .unwrap()
        .profiles()["epoch"]
        .clone();
    let base =
        json!({"profileId":"codex","projectId":id,"revision":configured["revision"],"epoch":epoch});
    let mut args = base.clone();
    args["workspaceId"] = json!(id);
    args["requestId"] = json!("create");
    let created = state
        .services
        .require::<nestos_node::harness::Harnesses>()
        .unwrap()
        .project_call(&state, 0, "harness_create", args)
        .await
        .unwrap();
    assert_eq!(created["outcome"], "committed", "{created}");
    assert_eq!(
        created["result"]["session"]["cwd"],
        format!("/projects/{id}")
    );
    if std::env::var("PI_AGENT_PROJECT_CODEX").as_deref() != Ok("1") {
        let session = created["result"]["session"]["id"].clone();
        let mut args = base.clone();
        args["sessionId"] = session.clone();
        args["requestId"] = json!("turn");
        args["prompt"] = json!("test");
        let turn = state
            .services
            .require::<nestos_node::harness::Harnesses>()
            .unwrap()
            .project_call(&state, 0, "harness_turn", args)
            .await
            .unwrap();
        assert_eq!(turn["outcome"], "committed");
        assert!(state.files.shared().is_err());
        assert_eq!(
            call(
                &state,
                0,
                "project_configure",
                json!({"projectId":id,"revision":2,"name":"Notes","mounts":[]})
            )
            .await
            .unwrap_err()
            .code,
            "EBUSY"
        );
        let mut args = base.clone();
        args["sessionId"] = session;
        args["turnId"] = turn["result"]["turnId"].clone();
        args["requestId"] = json!("interrupt");
        assert_eq!(
            state
                .services
                .require::<nestos_node::harness::Harnesses>()
                .unwrap()
                .project_call(&state, 0, "harness_interrupt", args)
                .await
                .unwrap()["outcome"],
            "committed"
        );
        assert!(state.files.shared().is_ok());
    }
    let other = register(&state, "group/new", true).await;
    let mut args = base.clone();
    args["projectId"] = other["id"].clone();
    args["revision"] = json!(1);
    args["sessionId"] = created["result"]["session"]["id"].clone();
    assert!(state
        .services
        .require::<nestos_node::harness::Harnesses>()
        .unwrap()
        .project_call(&state, 0, "harness_session_read", args)
        .await
        .is_err());
    state
        .services
        .require::<nestos_node::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
}

fn patch_note(export: &nestos_node::fs::Export, path: &str) {
    export
        .apply(
            &nestos_node::fs::mutation::Change::Metadata {
                path: path.into(),
                patch: json!({"_document_chat":{"session":"stable-session"}})
                    .as_object()
                    .unwrap()
                    .clone(),
                remove: vec![],
            },
            || Ok(()),
        )
        .unwrap();
}

#[tokio::test]
async fn project_scope_moves_ancestor_metadata_and_preserves_identity_across_remount() {
    use nestos_node::fs::mutation::Change;
    let root = tempfile::tempdir().unwrap();
    let config = deployment(root.path());
    let state = State::from_config(&config).unwrap();
    let export = state.exports.get("home").unwrap();
    let path = "group/existing/note.md";
    std::fs::write(root.path().join("exports").join(path), "note").unwrap();
    patch_note(export, path);
    let before = export.stat(path).unwrap().unwrap();
    let project = register(&state, "group/existing", false).await;
    let scoped = root
        .path()
        .join("exports/group/existing/.nestos/meta.sqlite");
    let db = rusqlite::Connection::open(&scoped).unwrap();
    let (relative, identity): (String, String) = db
        .query_row("SELECT path,identity FROM file_metadata", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(relative, "note.md");
    assert_eq!(Some(identity), before.source_identity);
    let parent =
        rusqlite::Connection::open(root.path().join("exports/.nestos/meta.sqlite")).unwrap();
    assert_eq!(
        parent
            .query_row::<i64, _, _>("SELECT count(*) FROM file_metadata", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        export.stat(path).unwrap().unwrap().metadata,
        before.metadata
    );
    assert_eq!(
        export
            .apply(
                &Change::Rename {
                    path: path.into(),
                    to: "outside.md".into()
                },
                || Ok(())
            )
            .unwrap_err()
            .code,
        "METADATA_SCOPE_CROSS_MOVE"
    );
    assert!(root.path().join("exports").join(path).exists());
    export
        .apply(
            &Change::Rename {
                path: path.into(),
                to: "group/existing/renamed.md".into(),
            },
            || Ok(()),
        )
        .unwrap();
    let readonly = remount(&state, &project, "ro").await.unwrap()["project"].clone();
    remount(&state, &readonly, "rw").await.unwrap();
    assert_eq!(
        export
            .stat("group/existing/renamed.md")
            .unwrap()
            .unwrap()
            .source_identity,
        before.source_identity
    );
}

#[tokio::test]
async fn project_scope_migrates_legacy_directory_and_keeps_owner_records_separate() {
    let root = tempfile::tempdir().unwrap();
    let config = deployment(root.path());
    let project_root = root.path().join("exports/group/existing");
    std::fs::write(project_root.join("note.md"), "note").unwrap();
    {
        let export = nestos_node::fs::Export::exclusive(&project_root).unwrap();
        patch_note(&export, "note.md");
    }
    std::fs::write(project_root.join(".nestos/info.seq"), "preserved-sequence").unwrap();
    std::fs::rename(project_root.join(".nestos"), project_root.join(".mindos")).unwrap();
    let state = State::from_config(&config).unwrap();
    let project = register(&state, "group/existing", false).await;
    assert!(!project_root.join(".mindos").exists());
    assert_eq!(
        std::fs::read_to_string(project_root.join(".nestos/info.seq")).unwrap(),
        "preserved-sequence"
    );
    let export = state.exports.get("home").unwrap();
    let id = project["id"].as_str().unwrap();
    export
        .initialize_metadata_scope(
            "group/existing",
            "other-server:other-owner",
            id,
            &json!({"name":"Other"}),
        )
        .unwrap();
    let db = rusqlite::Connection::open(project_root.join(".nestos/meta.sqlite")).unwrap();
    let names: Vec<String> = db
        .prepare("SELECT json_extract(value,'$.name') FROM scope_info ORDER BY owner")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(names, vec!["Other", "Notes"]);
    assert_eq!(
        export
            .stat("group/existing/note.md")
            .unwrap()
            .unwrap()
            .metadata["_document_chat"]["session"],
        "stable-session"
    );
}

#[tokio::test]
async fn readonly_project_does_not_create_metadata_directory() {
    let root = tempfile::tempdir().unwrap();
    let state = State::from_config(&deployment(root.path())).unwrap();
    call(
        &state,
        0,
        "project_register",
        json!({"name":"Reference","alias":"home","path":"group/existing","access":"ro"}),
    )
    .await
    .unwrap();
    assert!(!root.path().join("exports/group/existing/.nestos").exists());
    assert!(!root.path().join("exports/group/existing/.mindos").exists());
}
