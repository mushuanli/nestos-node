use cells::{app::State, config::Config, projects::call, sync::SyncService};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Write, os::unix::fs::PermissionsExt, sync::Arc};
fn deployment(root: &std::path::Path) -> Config {
    for dir in ["exports/project/sub", "reference", "sync", "catalog"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let config: Config = toml::from_str(&format!(
        r#"
listen="127.0.0.1:0"
execution=false
server_id="directory-test"
token="directory-test-token-0123456789"
[projects]
root={catalog:?}
[[exports]]
alias="home"
path={exports:?}
access="rw"
[[exports]]
alias="reference"
path={reference:?}
[sync]
enabled=true
root={sync:?}
metadata_reserve_bytes=0
"#,
        catalog = root.join("catalog"),
        exports = root.join("exports"),
        reference = root.join("reference"),
        sync = root.join("sync")
    ))
    .unwrap();
    SyncService::init(config.sync.as_ref().unwrap()).unwrap();
    config
}
fn command(s: &SyncService, seq: u64, extra: Value) -> Value {
    let mut v = json!({"authorityId":s.capabilities()["authorityId"],"historyEpoch":s.epoch(),"replicaId":"A","operationId":format!("A-{seq}"),"opSeq":seq.to_string(),"expectedProjectLifecycleRevision":"1"});
    for (k, value) in extra.as_object().unwrap() {
        v[k] = value.clone();
    }
    v
}
fn upload(s: &SyncService, bytes: &[u8]) -> String {
    let hash = format!("{:x}", Sha256::digest(bytes));
    let id = s.reserve("p", &hash, bytes.len() as u64).unwrap();
    let mut file = s.temporary_file(&id).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
    drop(file);
    s.install(&id).unwrap();
    hash
}
fn publish(state: &State, seq: u64, entries: Value) -> String {
    let s = state
        .services
        .get::<cells::sync::SyncService>()
        .unwrap();
    let manifest = upload(
        s,
        &serde_json::to_vec(&json!({"format":"fs-agent.files","version":1,"entries":entries}))
            .unwrap(),
    );
    let (target, extra) = if seq == 2 {
        (
            "projects/p/datasets",
            json!({"datasetId":"files","kind":"files","logicalId":"files","manifestHash":manifest}),
        )
    } else {
        (
            "projects/p/datasets/files/publish",
            json!({"expectedHead":s.head("p","files").unwrap(),"nextManifestHash":manifest}),
        )
    };
    // The command head schema excludes lifecycle and state metadata.
    let mut extra = extra;
    if seq != 2 {
        let head = s.head("p", "files").unwrap();
        extra["expectedHead"] =
            json!({"generation":head["generation"],"manifestHash":head["manifestHash"]});
    }
    let reply = s.command(target, &command(s, seq, extra), false).unwrap();
    assert_eq!(reply["outcome"], "committed", "{reply}");
    manifest
}
async fn setup(root: &std::path::Path) -> (Config, Arc<State>, Value) {
    let config = deployment(root);
    let state = State::from_config(&config).unwrap();
    let s = state
        .services
        .get::<cells::sync::SyncService>()
        .unwrap();
    s.register(&json!({"replicaId":"A"})).unwrap();
    s.activate("A", &json!({"scopes":[]})).unwrap();
    assert_eq!(
        s.command("projects", &command(s, 1, json!({"projectId":"p"})), false)
            .unwrap()["outcome"],
        "committed"
    );
    let project = call(
        &state,
        0,
        "project_register",
        json!({"name":"Directory","alias":"home","path":"project","access":"rw"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    (config, state, project)
}
async fn bind(state: &Arc<State>, project: &Value, target: &str) -> Value {
    call(state,0,"project_sync_bind",json!({"bindingId":"binding","projectId":project["id"],"revision":project["revision"],"syncProjectId":"p","datasetId":"files","historyEpoch":state.services.get::<cells::sync::SyncService>().unwrap().epoch(),"target":target})).await.unwrap()
}
async fn preview(state: &Arc<State>) -> Value {
    call(
        state,
        0,
        "project_sync_preview",
        json!({"bindingId":"binding"}),
    )
    .await
    .unwrap()["plan"]
        .clone()
}
async fn execute(state: &Arc<State>, id: &Value) -> Result<Value, cells::core::error::Error> {
    call(
        state,
        0,
        "project_sync_execute",
        json!({"bindingId":"binding","planId":id}),
    )
    .await
}
#[tokio::test]
async fn directory_sync_is_incremental_durable_and_never_propagates_deletion() {
    let root = tempfile::tempdir().unwrap();
    let (config, state, project) = setup(root.path()).await;
    let hash = upload(
        state
            .services
            .get::<cells::sync::SyncService>()
            .unwrap(),
        b"#!/bin/sh\necho hello\n",
    );
    publish(
        &state,
        2,
        json!([{"kind":"directory","path":"bin"},{"kind":"file","path":"bin/run","hash":hash,"size":"21","executable":true}]),
    );
    bind(&state, &project, "sub").await;
    let plan = preview(&state).await;
    assert_eq!(plan["actions"].as_array().unwrap().len(), 2);
    assert!(!root.path().join("exports/project/sub/bin").exists());
    execute(&state, &plan["id"]).await.unwrap();
    execute(&state, &plan["id"]).await.unwrap();
    let path = root.path().join("exports/project/sub/bin/run");
    assert!(path.exists());
    assert_ne!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o111,
        0
    );
    drop(state);
    let state = State::from_config(&config).unwrap();
    assert!(preview(&state).await["actions"]
        .as_array()
        .unwrap()
        .is_empty());
    publish(&state, 3, json!([]));
    let plan = preview(&state).await;
    execute(&state, &plan["id"]).await.unwrap();
    assert!(path.exists());
}
#[tokio::test]
async fn target_edits_and_stale_heads_are_rejected_before_writing() {
    let root = tempfile::tempdir().unwrap();
    let (_, state, project) = setup(root.path()).await;
    let s = state
        .services
        .get::<cells::sync::SyncService>()
        .unwrap();
    let hash = upload(s, b"local");
    publish(
        &state,
        2,
        json!([{"kind":"file","path":"a.txt","hash":hash,"size":"5"}]),
    );
    bind(&state, &project, "sub").await;
    let plan = preview(&state).await;
    std::fs::write(root.path().join("exports/project/sub/a.txt"), "codex").unwrap();
    assert_eq!(
        execute(&state, &plan["id"]).await.unwrap_err().code,
        "TARGET_CHANGED"
    );
    assert_eq!(preview(&state).await["conflicts"], json!(["a.txt"]));
    std::fs::remove_file(root.path().join("exports/project/sub/a.txt")).unwrap();
    let plan = preview(&state).await;
    publish(&state, 3, json!([]));
    assert_eq!(
        execute(&state, &plan["id"]).await.unwrap_err().code,
        "SYNC_SOURCE_CHANGED"
    );
}
#[tokio::test]
async fn directory_grants_are_revision_fenced_and_do_not_cross_mounts() {
    let root = tempfile::tempdir().unwrap();
    let (_, state, project) = setup(root.path()).await;
    publish(&state, 2, json!([{"kind":"directory","path":".nestos"}]));
    bind(&state, &project, "sub").await;
    let plan = preview(&state).await;
    assert_eq!(plan["conflicts"], json!([".nestos"]));
    assert_eq!(
        execute(&state, &plan["id"]).await.unwrap_err().code,
        "SYNC_CONFLICT"
    );
    let gate = state.files.exclusive().unwrap();
    assert_eq!(
        call(
            &state,
            0,
            "project_sync_preview",
            json!({"bindingId":"binding"})
        )
        .await
        .unwrap_err()
        .code,
        "EBUSY"
    );
    drop(gate);
    std::fs::rename(
        root.path().join("exports/project/sub"),
        root.path().join("exports/project/saved"),
    )
    .unwrap();
    std::fs::create_dir(root.path().join("exports/project/sub")).unwrap();
    assert_eq!(
        call(
            &state,
            0,
            "project_sync_preview",
            json!({"bindingId":"binding"})
        )
        .await
        .unwrap_err()
        .code,
        "TARGET_REPLACED"
    );
}

#[tokio::test]
async fn resumes_a_published_file_before_its_baseline_checkpoint_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let (config, state, project) = setup(root.path()).await;
    let hash = upload(
        state
            .services
            .get::<cells::sync::SyncService>()
            .unwrap(),
        b"new",
    );
    publish(
        &state,
        2,
        json!([{"kind":"file","path":"a.txt","hash":hash,"size":"3"},{"kind":"file","path":"b.txt","hash":hash,"size":"3"}]),
    );
    bind(&state, &project, "sub").await;
    let plan = preview(&state).await;
    drop(state);
    // Reconstruct the durable boundary: applying journal exists, first rename
    // reached disk, but its baseline checkpoint did not complete.
    let path = root.path().join("catalog/projects.json");
    let mut image: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    image["bindings"]["binding"]["plan"]["state"] = json!("applying");
    std::fs::write(&path, serde_json::to_vec(&image).unwrap()).unwrap();
    std::fs::write(root.path().join("exports/project/sub/a.txt"), "new").unwrap();
    let state = State::from_config(&config).unwrap();
    assert_eq!(
        call(
            &state,
            0,
            "project_sync_preview",
            json!({"bindingId":"binding"})
        )
        .await
        .unwrap_err()
        .code,
        "SYNC_APPLY_PENDING"
    );
    execute(&state, &plan["id"]).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("exports/project/sub/b.txt")).unwrap(),
        "new"
    );
    assert_eq!(
        call(
            &state,
            0,
            "project_sync_status",
            json!({"bindingId":"binding"})
        )
        .await
        .unwrap()["binding"]["plan"]["state"],
        "complete"
    );
}

#[tokio::test]
async fn readonly_mounts_policy_changes_and_duplicate_targets_cannot_expand_sync_access() {
    let root = tempfile::tempdir().unwrap();
    let (_, state, project) = setup(root.path()).await;
    publish(&state, 2, json!([]));
    bind(&state, &project, "sub").await;
    let mut input = json!({"bindingId":"other","projectId":project["id"],"revision":project["revision"],"syncProjectId":"p","datasetId":"files","historyEpoch":state.services.get::<cells::sync::SyncService>().unwrap().epoch(),"target":""});
    assert_eq!(
        call(&state, 0, "project_sync_bind", input.clone())
            .await
            .unwrap_err()
            .code,
        "TARGET_ALREADY_BOUND"
    );
    input["target"] = json!("../escape");
    assert_eq!(
        call(&state, 0, "project_sync_bind", input)
            .await
            .unwrap_err()
            .code,
        "EINVAL"
    );
    let configured=call(&state,0,"project_configure",json!({"projectId":project["id"],"revision":project["revision"],"name":"Mounted","mounts":[{"alias":"reference","path":"","at":"/workspace/sub","access":"ro"}]})).await.unwrap()["project"].clone();
    assert_eq!(
        call(
            &state,
            0,
            "project_sync_preview",
            json!({"bindingId":"binding"})
        )
        .await
        .unwrap_err()
        .code,
        "PROJECT_REVISION_CHANGED"
    );
    call(
        &state,
        0,
        "project_sync_unbind",
        json!({"bindingId":"binding"}),
    )
    .await
    .unwrap();
    let mounted = json!({"bindingId":"binding","projectId":configured["id"],"revision":configured["revision"],"syncProjectId":"p","datasetId":"files","historyEpoch":state.services.get::<cells::sync::SyncService>().unwrap().epoch(),"target":"sub"});
    assert_eq!(
        call(&state, 0, "project_sync_bind", mounted)
            .await
            .unwrap_err()
            .code,
        "SYNC_MOUNT_PATH"
    );
    publish(&state, 3, json!([{"kind":"directory","path":"sub"}]));
    bind(&state, &configured, "").await;
    assert_eq!(preview(&state).await["conflicts"], json!(["sub"]));
    let readonly = call(
        &state,
        0,
        "project_register",
        json!({"name":"Readonly","alias":"reference","path":"","access":"ro"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    let input = json!({"bindingId":"readonly","projectId":readonly["id"],"revision":readonly["revision"],"syncProjectId":"p","datasetId":"files","historyEpoch":state.services.get::<cells::sync::SyncService>().unwrap().epoch(),"target":""});
    assert_eq!(
        call(&state, 0, "project_sync_bind", input.clone())
            .await
            .unwrap_err()
            .code,
        "EROFS"
    );
    assert_eq!(
        call(&state, 1, "project_sync_bind", input)
            .await
            .unwrap_err()
            .code,
        "EACCES"
    );
}

async fn configure(state: &Arc<State>, revision: u64, direction: &str) -> Value {
    call(
        state,
        0,
        "project_sync_configure",
        json!({"bindingId":"binding","policyRevision":revision,"direction":direction}),
    )
    .await
    .unwrap()["binding"]
        .clone()
}
fn dataset_contents(state: &State) -> Value {
    let sync = state
        .services
        .get::<cells::sync::SyncService>()
        .unwrap();
    let head = sync.directory_head("p", "files").unwrap();
    serde_json::from_slice(
        &sync
            .manifest_bytes("p", head["manifestHash"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap()
}
#[tokio::test]
async fn both_directions_converge_independent_edits_and_keep_unique_files() {
    let root = tempfile::tempdir().unwrap();
    let (_, state, project) = setup(root.path()).await;
    let first = upload(
        state
            .services
            .get::<cells::sync::SyncService>()
            .unwrap(),
        b"original",
    );
    publish(
        &state,
        2,
        json!([{"kind":"file","path":"note","hash":first,"size":"8"}]),
    );
    bind(&state, &project, "sub").await;
    let initial = preview(&state).await;
    execute(&state, &initial["id"]).await.unwrap();
    configure(&state, 1, "both").await;
    std::fs::write(root.path().join("exports/project/sub/note"), "native edit").unwrap();
    std::fs::write(
        root.path().join("exports/project/sub/native-only"),
        "native",
    )
    .unwrap();
    let other = upload(
        state
            .services
            .get::<cells::sync::SyncService>()
            .unwrap(),
        b"cloud",
    );
    publish(
        &state,
        3,
        json!([{"kind":"file","path":"cloud-only","hash":other,"size":"5"},{"kind":"file","path":"note","hash":first,"size":"8"}]),
    );
    let plan = preview(&state).await;
    assert_eq!(plan["conflicts"], json!([]));
    assert!(plan["actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["path"] == "note" && a["side"] == "upload"));
    assert!(plan["actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["path"] == "cloud-only" && a["side"] == "download"));
    execute(&state, &plan["id"]).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("exports/project/sub/cloud-only")).unwrap(),
        "cloud"
    );
    let entries = dataset_contents(&state)["entries"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(entries.len(), 3);
    assert_eq!(
        entries.iter().find(|e| e["path"] == "note").unwrap()["hash"],
        format!("{:x}", Sha256::digest(b"native edit"))
    );
    assert!(preview(&state).await["actions"]
        .as_array()
        .unwrap()
        .is_empty());
}
#[tokio::test]
async fn conflict_content_is_readable_and_source_selection_creates_a_new_review() {
    let root = tempfile::tempdir().unwrap();
    let (_, state, project) = setup(root.path()).await;
    let original = upload(
        state
            .services
            .get::<cells::sync::SyncService>()
            .unwrap(),
        b"baseline\n",
    );
    publish(
        &state,
        2,
        json!([{"kind":"file","path":"note","hash":original,"size":"9"}]),
    );
    bind(&state, &project, "sub").await;
    let first = preview(&state).await;
    execute(&state, &first["id"]).await.unwrap();
    configure(&state, 1, "both").await;
    std::fs::write(root.path().join("exports/project/sub/note"), "native\n").unwrap();
    let hash = upload(
        state
            .services
            .get::<cells::sync::SyncService>()
            .unwrap(),
        b"dataset\n",
    );
    publish(
        &state,
        3,
        json!([{"kind":"file","path":"note","hash":hash,"size":"8"}]),
    );
    let plan = preview(&state).await;
    assert_eq!(plan["conflictDetails"][0]["code"], "CONTENT_CONFLICT");
    let contents = call(
        &state,
        0,
        "project_sync_compare",
        json!({"bindingId":"binding","planId":plan["id"],"path":"note"}),
    )
    .await
    .unwrap();
    assert_eq!(contents["baseline"]["content"]["text"], "baseline\n");
    assert_eq!(contents["dataset"]["content"]["text"], "dataset\n");
    assert_eq!(contents["directory"]["content"]["text"], "native\n");
    let resolved = call(
        &state,
        0,
        "project_sync_resolve",
        json!({"bindingId":"binding","planId":plan["id"],"decisions":{"note":"directory"}}),
    )
    .await
    .unwrap()["plan"]
        .clone();
    assert_ne!(resolved["id"], plan["id"]);
    assert_eq!(resolved["conflicts"], json!([]));
    assert_eq!(dataset_contents(&state)["entries"][0]["hash"], hash);
    assert_eq!(
        execute(&state, &plan["id"]).await.unwrap_err().code,
        "PLAN_CHANGED"
    );
    execute(&state, &resolved["id"]).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("exports/project/sub/note")).unwrap(),
        "native\n"
    );
    assert!(preview(&state).await["conflicts"]
        .as_array()
        .unwrap()
        .is_empty());
}
#[tokio::test]
async fn direction_and_input_changes_invalidate_plans_without_publishing() {
    let root = tempfile::tempdir().unwrap();
    let (_, state, project) = setup(root.path()).await;
    publish(&state, 2, json!([]));
    bind(&state, &project, "sub").await;
    configure(&state, 1, "upload").await;
    std::fs::write(root.path().join("exports/project/sub/native"), "first").unwrap();
    let plan = preview(&state).await;
    std::fs::write(root.path().join("exports/project/sub/later"), "later").unwrap();
    assert_eq!(
        execute(&state, &plan["id"]).await.unwrap_err().code,
        "TARGET_CHANGED"
    );
    assert_eq!(dataset_contents(&state)["entries"], json!([]));
    configure(&state, 2, "download").await;
    assert_eq!(
        execute(&state, &plan["id"]).await.unwrap_err().code,
        "PLAN_CHANGED"
    );
    assert_eq!(
        call(
            &state,
            0,
            "project_sync_configure",
            json!({"bindingId":"binding","policyRevision":2,"direction":"both"})
        )
        .await
        .unwrap_err()
        .code,
        "BINDING_CHANGED"
    );
    assert!(preview(&state).await["actions"]
        .as_array()
        .unwrap()
        .is_empty());
}
#[tokio::test]
async fn recovers_original_upload_receipt_after_restart_even_when_the_head_advances() {
    let root = tempfile::tempdir().unwrap();
    let (config, state, project) = setup(root.path()).await;
    publish(&state, 2, json!([]));
    bind(&state, &project, "sub").await;
    configure(&state, 1, "upload").await;
    std::fs::write(root.path().join("exports/project/sub/note"), "native").unwrap();
    let plan = preview(&state).await;
    execute(&state, &plan["id"]).await.unwrap();
    let other = upload(
        state
            .services
            .get::<cells::sync::SyncService>()
            .unwrap(),
        b"later cloud",
    );
    publish(
        &state,
        3,
        json!([{"kind":"file","path":"note","hash":other,"size":"11"}]),
    );
    drop(state);
    let path = root.path().join("catalog/projects.json");
    let mut image: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    image["bindings"]["binding"]["plan"]["state"] = json!("applying");
    image["bindings"]["binding"]["baseline"] = json!({});
    std::fs::write(&path, serde_json::to_vec(&image).unwrap()).unwrap();
    let state = State::from_config(&config).unwrap();
    execute(&state, &plan["id"]).await.unwrap();
    assert_eq!(dataset_contents(&state)["entries"][0]["hash"], other);
    assert_eq!(preview(&state).await["conflicts"], json!(["note"]));
}
#[tokio::test]
async fn compares_binary_limits_and_browses_only_authorized_project_directories() {
    let root = tempfile::tempdir().unwrap();
    let (_, state, project) = setup(root.path()).await;
    let hash = upload(
        state
            .services
            .get::<cells::sync::SyncService>()
            .unwrap(),
        b"data\0",
    );
    publish(
        &state,
        2,
        json!([{"kind":"file","path":"note","hash":hash,"size":"5"}]),
    );
    std::fs::write(
        root.path().join("exports/project/sub/note"),
        vec![b'x'; 256 * 1024 + 1],
    )
    .unwrap();
    bind(&state, &project, "sub").await;
    let plan = preview(&state).await;
    let contents = call(
        &state,
        0,
        "project_sync_compare",
        json!({"bindingId":"binding","planId":plan["id"],"path":"note"}),
    )
    .await
    .unwrap();
    assert_eq!(contents["dataset"]["content"]["reason"], "binary");
    assert_eq!(contents["directory"]["content"]["reason"], "too-large");
    let dirs = call(
        &state,
        0,
        "project_sync_directories",
        json!({"projectId":project["id"],"revision":project["revision"],"path":""}),
    )
    .await
    .unwrap();
    assert_eq!(dirs["paths"], json!(["sub"]));
    assert_eq!(
        call(
            &state,
            0,
            "project_sync_directories",
            json!({"projectId":project["id"],"revision":project["revision"],"path":"../reference"})
        )
        .await
        .unwrap_err()
        .code,
        "EINVAL"
    );
    assert_eq!(
        call(
            &state,
            0,
            "project_sync_resolve",
            json!({"bindingId":"binding","planId":plan["id"],"decisions":{"note":"directory"}})
        )
        .await
        .unwrap_err()
        .code,
        "EINVAL"
    );
    assert!(call(
        &state,
        0,
        "project_sync_compare",
        json!({"bindingId":"binding","planId":plan["id"],"path":"../../secret"})
    )
    .await
    .is_err());
}
