use cells::{app::State, config::Config, projects::call};
use serde_json::{json, Value};
use std::{os::unix::fs::symlink, sync::Arc};
fn fixture() -> (tempfile::TempDir, Arc<State>) {
    let root = tempfile::tempdir().unwrap();
    for path in [
        "export/project/ref",
        "export/project/private-native",
        "reference",
        "catalog",
    ] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    let config:Config=toml::from_str(&format!("listen=\"127.0.0.1:0\"\nexecution=false\nserver_id=\"watch-test\"\ntoken=\"watch-fixture-token-0123456789\"\n[projects]\nroot={catalog:?}\n[[exports]]\nalias=\"root\"\npath={export:?}\naccess=\"rw\"\n[[exports]]\nalias=\"reference\"\npath={reference:?}\n[[harnesses]]\nid=\"codex\"\nkind=\"codex\"\nhome={native:?}\nprojects=true\n",catalog=root.path().join("catalog"),export=root.path().join("export"),reference=root.path().join("reference"),native=root.path().join("export/project/private-native"))).unwrap();
    let state = State::from_config(&config).unwrap();
    (root, state)
}
async fn project(state: &Arc<State>) -> Value {
    call(state,0,"project_register",json!({"name":"P","alias":"root","path":"project","access":"ro","mounts":[{"alias":"reference","path":"","at":"/workspace/ref","access":"ro"}]})).await.unwrap()["project"].clone()
}
#[tokio::test]
async fn subscription_waits_for_kernel_changes_and_releases_its_watch() {
    use futures_util::StreamExt;
    let (root, state) = fixture();
    let project = project(&state).await;
    let args = json!({"projectId":project["id"],"revision":project["revision"]});
    let mut source = state
        .plugins
        .subscribe(&state, 0, "projects", args.clone())
        .await
        .unwrap();
    let first = source.next().await.unwrap().unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(40), source.next())
            .await
            .is_err()
    );
    std::fs::write(
        root.path().join("export/project/from-cli.md"),
        "external change",
    )
    .unwrap();
    let next = tokio::time::timeout(std::time::Duration::from_secs(2), source.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_ne!(next["version"], first["version"]);
    assert!(state
        .plugins
        .subscribe(&state, 1, "projects", args.clone())
        .await
        .is_err());
    let mut stale = args.clone();
    stale["revision"] = json!(999);
    assert!(state
        .plugins
        .subscribe(&state, 0, "projects", stale)
        .await
        .is_err());
    drop(source);
    for _ in 0..20 {
        drop(
            state
                .plugins
                .subscribe(&state, 0, "projects", args.clone())
                .await
                .unwrap(),
        );
    }
}
async fn next(source: &mut cells::plugins::EventStream) -> Value {
    use futures_util::StreamExt;
    tokio::time::timeout(std::time::Duration::from_secs(2), source.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
#[tokio::test]
async fn detects_external_writes_without_exposing_private_or_shadowed_trees() {
    use futures_util::StreamExt;
    let (root, state) = fixture();
    let project = project(&state).await;
    let args = json!({"projectId":project["id"],"revision":project["revision"]});
    let mut source = state
        .plugins
        .subscribe(&state, 0, "projects", args)
        .await
        .unwrap();
    let first = next(&mut source).await;
    assert_eq!(first["truncated"], false);
    let base = root.path().join("export/project");
    std::fs::write(base.join("external.md"), "external").unwrap();
    let changed = next(&mut source).await;
    assert_ne!(changed["version"], first["version"]);
    std::fs::write(base.join("ref/shadowed.md"), "shadow").unwrap();
    std::fs::write(base.join("private-native/auth.json"), "private").unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(40), source.next())
            .await
            .is_err()
    );
    std::fs::write(root.path().join("reference/visible.md"), "mounted").unwrap();
    let mounted = next(&mut source).await;
    assert_ne!(mounted["version"], changed["version"]);
    std::fs::create_dir(base.join("new")).unwrap();
    next(&mut source).await;
    std::fs::write(base.join("new/nested.md"), "nested").unwrap();
    next(&mut source).await;
    symlink(root.path().join("reference"), base.join("link")).unwrap();
    next(&mut source).await;
    drop(source);
    for _ in 0..20 {
        drop(
            state
                .plugins
                .subscribe(
                    &state,
                    0,
                    "projects",
                    json!({"projectId":project["id"],"revision":project["revision"]}),
                )
                .await
                .unwrap(),
        );
    }
}
#[tokio::test]
async fn fences_subscriptions_by_owner_revision_and_directory_identity() {
    use futures_util::StreamExt;
    let (root, state) = fixture();
    let project = project(&state).await;
    let args = json!({"projectId":project["id"],"revision":project["revision"]});
    assert!(state
        .plugins
        .subscribe(&state, 1, "projects", args.clone())
        .await
        .is_err());
    let mut stale = args.clone();
    stale["revision"] = json!(999);
    assert!(state
        .plugins
        .subscribe(&state, 0, "projects", stale)
        .await
        .is_err());
    let mut source = state
        .plugins
        .subscribe(&state, 0, "projects", args)
        .await
        .unwrap();
    next(&mut source).await;
    std::fs::rename(
        root.path().join("export/project"),
        root.path().join("export/old"),
    )
    .unwrap();
    std::fs::create_dir(root.path().join("export/project")).unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), source.next())
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}
