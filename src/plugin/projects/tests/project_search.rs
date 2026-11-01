use cells::{app::State, config::Config, projects::call};
use serde_json::{json, Value};
use std::{os::unix::fs::symlink, sync::Arc};

// Independent test servers share the process-wide search capacity.
static TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn fixture() -> (
    tempfile::TempDir,
    Arc<State>,
    Value,
    tokio::sync::MutexGuard<'static, ()>,
) {
    let guard = TESTS.lock().await;
    let root = tempfile::tempdir().unwrap();
    for path in ["export/project/ref", "reference", "catalog", "native"] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    let config: Config = toml::from_str(&format!(
        r#"
listen="127.0.0.1:0"
execution=true
server_id="search-test"
token="search-test-secret-0123456789"
[projects]
root={catalog:?}
[[exports]]
alias="root"
path={export:?}
access="rw"
[[exports]]
alias="reference"
path={reference:?}
[[harnesses]]
id="codex"
kind="codex"
home={native:?}
projects=true
"#,
        catalog = root.path().join("catalog"),
        export = root.path().join("export"),
        reference = root.path().join("reference"),
        native = root.path().join("native")
    ))
    .unwrap();
    let state = State::from_config(&config).unwrap();
    state
        .services
        .require::<cells::process::Execution>()
        .unwrap()
        .retain_lock(std::fs::File::open(root.path()).unwrap());
    state
        .services
        .require::<cells::process::Execution>()
        .unwrap()
        .accept();
    let project = call(
        &state,
        0,
        "project_register",
        json!({"name":"P","alias":"root","path":"project","access":"ro",
        "mounts":[{"alias":"reference","path":"","at":"/workspace/ref","access":"ro"}]}),
    )
    .await
    .unwrap()["project"]
        .clone();
    (root, state, project, guard)
}
async fn search(
    state: &Arc<State>,
    project: &Value,
    query: &str,
    mode: &str,
) -> Result<Value, cells::core::error::Error> {
    call(
        state,
        0,
        "project_search",
        json!({"projectId":project["id"],"revision":project["revision"],"query":query,"mode":mode}),
    )
    .await
}
async fn scoped_search(
    state: &Arc<State>,
    project: &Value,
    query: &str,
    mode: &str,
    directory: &str,
) -> Result<Value, cells::core::error::Error> {
    call(
        state,
        0,
        "project_search",
        json!({"projectId":project["id"],"revision":project["revision"],
        "query":query,"mode":mode,"directory":directory}),
    )
    .await
}
#[tokio::test]
async fn keyword_and_search_filters_before_result_limits_and_keeps_names_distinct_from_paths() {
    let (root, state, project, _guard) = fixture().await;
    let directory = root.path().join("export/project/Alpha");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(
        directory.join("Beta-a.*.txt"),
        "longest-only\n".repeat(150) + "SECOND longest-only 中文\nlongest-only\nSECOND",
    )
    .unwrap();
    let names = search(&state, &project, "a.*　BETA beta", "name")
        .await
        .unwrap();
    assert_eq!(names["matches"].as_array().unwrap().len(), 1);
    assert_eq!(names["matches"][0]["path"], "Alpha/Beta-a.*.txt");
    assert_eq!(
        search(&state, &project, "beta Alpha", "name")
            .await
            .unwrap()["matches"],
        json!([])
    );
    assert_eq!(
        search(&state, &project, "beta\tALPHA", "path")
            .await
            .unwrap()["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let content = search(&state, &project, "second longest-only 中文", "content")
        .await
        .unwrap();
    assert_eq!(content["matches"].as_array().unwrap().len(), 1, "{content}");
    assert_eq!(content["matches"][0]["line"], 151);
    assert_eq!(content["truncated"], false);
    assert_eq!(
        search(&state, &project, "second absent", "content")
            .await
            .unwrap()["matches"],
        json!([])
    );
}
#[tokio::test]
async fn searches_only_the_readonly_project_view_and_applies_ignore_and_private_policies() {
    let (root, state, project, _guard) = fixture().await;
    let export = root.path().join("export/project");
    std::fs::write(export.join("ref/shadowed.txt"), "needle secret").unwrap();
    std::fs::write(root.path().join("reference/visible.txt"), "needle mount").unwrap();
    std::fs::write(export.join("visible\nfile.txt"), "before\nneedle main").unwrap();
    std::fs::write(export.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(export.join("ignored.txt"), "needle ignored").unwrap();
    std::fs::write(export.join("binary.bin"), b"needle\0binary").unwrap();
    std::fs::write(root.path().join("native/auth.json"), "needle private").unwrap();
    symlink(root.path().join("native"), export.join("external-link")).unwrap();
    let result = search(&state, &project, "needle", "content").await.unwrap();
    let paths: Vec<_> = result["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&"ref/visible.txt"));
    assert!(paths.contains(&"visible\nfile.txt"));
    assert_eq!(
        search(&state, &project, "before\nneedle", "content")
            .await
            .unwrap()["matches"][0]["line"],
        1
    );
    assert_eq!(
        search(&state, &project, "no-match", "content")
            .await
            .unwrap()["matches"],
        json!([])
    );
    assert_eq!(
        search(&state, &project, "-leading-query", "content")
            .await
            .unwrap()["matches"],
        json!([])
    );
    assert_eq!(
        search(&state, &project, "visible\n", "path").await.unwrap()["matches"][0]["path"],
        "visible\nfile.txt"
    );
}
#[tokio::test]
async fn rejects_revoked_revisions_and_replaced_directory_identities_before_search() {
    let (root, state, mut project, _guard) = fixture().await;
    project["revision"] = json!(999);
    assert_eq!(
        search(&state, &project, "needle", "content")
            .await
            .unwrap_err()
            .code,
        "PROJECT_REVISION_CHANGED"
    );
    project["revision"] = json!(1);
    std::fs::rename(
        root.path().join("export/project"),
        root.path().join("export/old"),
    )
    .unwrap();
    std::fs::create_dir(root.path().join("export/project")).unwrap();
    std::fs::create_dir(root.path().join("export/project/ref")).unwrap();
    assert_eq!(
        search(&state, &project, "needle", "content")
            .await
            .unwrap_err()
            .code,
        "PROJECT_DIRECTORY_CHANGED"
    );
}

#[tokio::test]
async fn filters_scoped_results_before_capacity_and_matches_only_basenames_in_name_mode() {
    let (root, state, project, _guard) = fixture().await;
    let export = root.path().join("export/project");
    std::fs::create_dir(export.join("needle")).unwrap();
    std::fs::write(export.join("needle/plain.txt"), "target in scope").unwrap();
    std::fs::write(export.join("needle.txt"), "target outside").unwrap();
    // More than one result page outside the directory must not hide an in-scope hit.
    for i in 0..120 {
        std::fs::write(export.join(format!("outside-{i}.txt")), "target outside").unwrap();
    }
    let scoped = |query: &str, mode: &str, directory: &str| json!({"projectId":project["id"],"revision":project["revision"],"query":query,"mode":mode,"directory":directory});
    let result = call(
        &state,
        0,
        "project_search",
        scoped("target", "content", "needle"),
    )
    .await
    .unwrap();
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    assert_eq!(result["matches"][0]["path"], "needle/plain.txt");
    assert_eq!(result["truncated"], false);
    assert_eq!(
        call(
            &state,
            0,
            "project_search",
            scoped("needle", "name", "needle")
        )
        .await
        .unwrap()["matches"],
        json!([])
    );
    assert_eq!(
        call(
            &state,
            0,
            "project_search",
            scoped("needle", "path", "needle")
        )
        .await
        .unwrap()["matches"][0]["path"],
        "needle/plain.txt"
    );
    for directory in ["../private", "/needle", "a//b", "needle/../other", "a\\b"] {
        assert!(call(
            &state,
            0,
            "project_search",
            scoped("target", "content", directory)
        )
        .await
        .is_err());
    }
}

#[tokio::test]
async fn scoped_search_prunes_unrelated_output_before_it_exhausts_the_scan_budget() {
    let (root, state, project, _guard) = fixture().await;
    let export = root.path().join("export/project");
    std::fs::create_dir_all(export.join("selected/deep")).unwrap();
    std::fs::write(export.join("selected/deep/target.txt"), "needle selected").unwrap();
    let noise = format!("needle {}\n", "x".repeat(18_000)).repeat(100);
    for i in 0..12 {
        std::fs::write(export.join(format!("noise-{i}.txt")), &noise).unwrap();
    }
    let result = scoped_search(&state, &project, "needle", "content", "selected/deep")
        .await
        .unwrap();
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    assert_eq!(result["matches"][0]["path"], "selected/deep/target.txt");
    assert_eq!(result["truncated"], false);
}

#[tokio::test]
async fn scoped_traversal_preserves_ignore_hidden_symlink_and_mount_boundaries() {
    let (root, state, project, _guard) = fixture().await;
    let export = root.path().join("export/project");
    for directory in [
        "selected/ignored",
        "selected/.hidden",
        "selected/node_modules",
        "ignored",
        ".hidden",
    ] {
        std::fs::create_dir_all(export.join(directory)).unwrap();
        std::fs::write(export.join(directory).join("note.txt"), "needle private").unwrap();
    }
    std::fs::write(export.join(".gitignore"), "ignored/\n").unwrap();
    std::fs::write(export.join("selected/.ignore"), "local-ignore.txt\n").unwrap();
    std::fs::write(export.join("selected/local-ignore.txt"), "needle ignored").unwrap();
    std::fs::write(export.join("selected/visible.txt"), "needle visible").unwrap();
    std::fs::write(export.join("ref/shadowed.txt"), "needle shadowed").unwrap();
    std::fs::write(root.path().join("reference/visible.txt"), "needle mounted").unwrap();
    symlink(export.join("selected"), export.join("link")).unwrap();
    for directory in [
        "ignored",
        "selected/ignored",
        ".hidden",
        "selected/.hidden",
        "selected/node_modules",
        "link",
    ] {
        assert_eq!(
            scoped_search(&state, &project, "needle", "content", directory)
                .await
                .unwrap()["matches"],
            json!([])
        );
    }
    for (directory, path) in [
        ("selected", "selected/visible.txt"),
        ("ref", "ref/visible.txt"),
    ] {
        let result = scoped_search(&state, &project, "needle", "content", directory)
            .await
            .unwrap();
        assert_eq!(result["matches"].as_array().unwrap().len(), 1);
        assert_eq!(result["matches"][0]["path"], path);
    }
}

#[tokio::test]
async fn scoped_directory_names_are_literal_and_keep_project_relative_paths() {
    let (root, state, project, _guard) = fixture().await;
    let export = root.path().join("export/project");
    for directory in ["[ab]{c}*? !# ", "plain", "line\nbreak"] {
        std::fs::create_dir(export.join(directory)).unwrap();
        std::fs::write(export.join(directory).join("note.txt"), "needle").unwrap();
    }
    for directory in ["[ab]{c}*? !# ", "line\nbreak"] {
        for mode in ["name", "path", "content"] {
            let query = if mode == "content" { "needle" } else { "note" };
            let result = scoped_search(&state, &project, query, mode, directory)
                .await
                .unwrap();
            let expected = if mode == "content" {
                json!([{"path":format!("{directory}/note.txt"),"line":1,"summary":"needle"}])
            } else {
                json!([{"path":format!("{directory}/note.txt"),"summary":format!("{directory}/note.txt")}])
            };
            assert_eq!(result["matches"], expected);
            assert_eq!(result["truncated"], false);
        }
    }
}
