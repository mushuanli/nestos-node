use nestos_node::{app::State, config::Config};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

const OWNER: &str = "owner-private-token-at-least-24-bytes";
const READER: &str = "reader-private-token-at-least-24-bytes";
struct Fixture {
    root: tempfile::TempDir,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared");
        fs::create_dir(&shared).unwrap();
        let path = root.path().join("config.toml");
        fs::write(&path,format!("# Operator comment\nlisten='127.0.0.1:0'\nserver_id='access-test'\nexecution=true\napi_key='{OWNER}'\nexports=[{{alias='docs',path='{}',access='rw'}}]\n",shared.display())).unwrap();
        Self { root, path }
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_nestos-node"))
            .args(["access", "--config"])
            .arg(&self.path)
            .args(args)
            .env("PI_READER_TEST_TOKEN", READER)
            .env("PI_BASIC_TEST_PASSWORD", "password-at-least-8-bytes")
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        no_secrets(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn text(&self) -> String {
        fs::read_to_string(&self.path).unwrap()
    }
}
fn no_secrets(output: &Output) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains(OWNER));
    assert!(!text.contains(READER));
    assert!(!text.contains("password-at-least-8-bytes"));
}

#[test]
fn migration_preserves_owner_access_credentials_and_private_backups() {
    let f = Fixture::new();
    let before = f.text();
    let result = f.ok(&["init"]);
    assert_eq!(result["restartRequired"], true);
    assert_eq!(result["policy"]["mode"], "principals");
    let backup = PathBuf::from(result["backup"].as_str().unwrap());
    assert_eq!(fs::read_to_string(&backup).unwrap(), before);
    for path in [&f.path, &backup] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(f.text().contains("# Operator comment"));
    let config: Config = toml::from_str(&f.text()).unwrap();
    assert!(config.token.is_none());
    assert_eq!(config.principals[0].id, "owner");
    assert_eq!(config.principals[0].token.as_deref(), Some(OWNER));
    for operation in ["read", "write", "execute"] {
        assert_eq!(
            f.ok(&["check", "owner", "docs", operation])["allowed"],
            true
        );
    }
    assert_eq!(f.ok(&["init"])["changed"], false);
}

#[test]
fn grants_public_access_and_execution_use_the_same_runtime_policy() {
    let f = Fixture::new();
    f.ok(&["init"]);
    f.ok(&[
        "principal",
        "add",
        "reader",
        "--token-env",
        "PI_READER_TEST_TOKEN",
    ]);
    f.ok(&["grant", "reader", "docs", "ro"]);
    assert_eq!(f.ok(&["check", "reader", "docs", "read"])["allowed"], true);
    for operation in ["write", "execute"] {
        let denied = f.run(&["check", "reader", "docs", operation]);
        assert!(!denied.status.success());
        no_secrets(&denied);
        assert_eq!(
            serde_json::from_slice::<Value>(&denied.stdout).unwrap()["allowed"],
            false
        );
    }
    f.ok(&["grant", "reader", "docs", "rw", "--execute"]);
    assert_eq!(
        f.ok(&["check", "reader", "docs", "execute"])["allowed"],
        true
    );
    f.ok(&["share", "set", "docs", "--public", "true"]);
    f.ok(&["revoke", "reader", "docs"]);
    assert_eq!(f.ok(&["check", "reader", "docs", "read"])["allowed"], true);
    assert_eq!(
        f.ok(&["check", "anonymous", "docs", "read"])["allowed"],
        true
    );
    for operation in ["write", "execute"] {
        assert!(!f
            .run(&["check", "anonymous", "docs", operation])
            .status
            .success());
    }
    let mut config: Config = toml::from_str(&f.text()).unwrap();
    for principal in &mut config.principals {
        if principal.token_env.take().is_some() {
            principal.token = Some(READER.into());
        }
    }
    let state = State::from_config(&config).unwrap();
    let anonymous = state.auth.clients().iter().find(|c| c.anonymous()).unwrap();
    assert!(anonymous.may_read("docs"));
    assert!(!anonymous.may_write("docs"));
    assert!(!anonymous.may_execute("docs"));
}

#[test]
fn dry_run_and_invalid_or_referenced_edits_leave_the_original_untouched() {
    let f = Fixture::new();
    f.ok(&["init"]);
    let before = f.text();
    let files = fs::read_dir(f.root.path()).unwrap().count();
    assert_eq!(
        f.ok(&[
            "--dry-run",
            "principal",
            "add",
            "reader",
            "--token-env",
            "PI_READER_TEST_TOKEN"
        ])["changed"],
        true
    );
    assert_eq!(f.text(), before);
    assert_eq!(fs::read_dir(f.root.path()).unwrap().count(), files);
    for args in [
        vec!["share", "set", "docs", "--access", "ro"],
        vec!["share", "remove", "docs"],
        vec!["principal", "remove", "owner"],
        vec!["grant", "missing", "docs", "rw"],
    ] {
        let output = f.run(&args);
        assert!(!output.status.success());
        no_secrets(&output);
        assert_eq!(f.text(), before);
    }
    f.ok(&[
        "principal",
        "add",
        "reader",
        "--token-env",
        "PI_READER_TEST_TOKEN",
    ]);
    f.ok(&["grant", "reader", "docs", "ro"]);
    let before = f.text();
    assert!(!f.run(&["principal", "remove", "reader"]).status.success());
    assert_eq!(f.text(), before);
    f.ok(&["revoke", "reader", "docs"]);
    f.ok(&["principal", "remove", "reader"]);
}

#[test]
fn checks_do_not_take_write_locks_initialize_stores_or_change_files() {
    let f = Fixture::new();
    let config: Config = toml::from_str(&f.text()).unwrap();
    let _state = State::from_config(&config).unwrap();
    let count = fs::read_dir(f.root.path()).unwrap().count();
    let before = f.text();
    assert_eq!(f.ok(&["validate"])["valid"], true);
    assert_eq!(f.ok(&["check", "owner", "docs", "write"])["allowed"], true);
    assert_eq!(f.text(), before);
    assert_eq!(fs::read_dir(f.root.path()).unwrap().count(), count);
}

#[test]
fn shares_basic_identity_and_credential_rotation_do_not_delete_data() {
    let f = Fixture::new();
    f.ok(&["init"]);
    let shared = f.root.path().join("second");
    fs::create_dir(&shared).unwrap();
    fs::write(shared.join("keep.txt"), "keep").unwrap();
    f.ok(&[
        "share",
        "add",
        "second",
        shared.to_str().unwrap(),
        "ro",
        "--public",
        "true",
    ]);
    f.ok(&[
        "principal",
        "add",
        "reader",
        "--username",
        "guest",
        "--password-env",
        "PI_BASIC_TEST_PASSWORD",
    ]);
    let denied = f.run(&["grant", "reader", "second", "rw"]);
    assert!(!denied.status.success());
    f.ok(&["grant", "reader", "second", "ro"]);
    f.ok(&[
        "principal",
        "credential",
        "reader",
        "--token-env",
        "PI_READER_TEST_TOKEN",
    ]);
    let summary = f.ok(&["list"]);
    assert!(summary["principals"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "reader" && p["authentication"] == "bearer"));
    f.ok(&["share", "remove", "second"]);
    assert_eq!(fs::read_to_string(shared.join("keep.txt")).unwrap(), "keep");
}

#[test]
fn help_works_without_config_and_errors_do_not_echo_configuration_or_cli_secrets() {
    let f = Fixture::new();
    let result = Command::new(env!("CARGO_BIN_EXE_nestos-node"))
        .args(["--help"])
        .current_dir(f.root.path())
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("access"));
    fs::write(&f.path, format!("token='{OWNER}'\ninvalid=[\n")).unwrap();
    let result = f.run(&["list"]);
    assert!(!result.status.success());
    no_secrets(&result);
    let result = f.run(&["principal", "add", "reader", "--token", READER]);
    assert!(!result.status.success());
    no_secrets(&result);
}

#[tokio::test]
async fn cli_directory_grants_are_enforced_by_a_real_http_listener() {
    let f = Fixture::new();
    f.ok(&["init"]);
    f.ok(&[
        "principal",
        "add",
        "reader",
        "--token-env",
        "PI_READER_TEST_TOKEN",
    ]);
    f.ok(&["grant", "reader", "docs", "ro"]);
    f.ok(&["share", "set", "docs", "--public", "true"]);
    let mut config: Config = toml::from_str(&f.text()).unwrap();
    let reader = config
        .principals
        .iter_mut()
        .find(|p| p.id == "reader")
        .unwrap();
    reader.token_env = None;
    reader.token = Some(READER.into());
    let state = State::from_config(&config).unwrap();
    let app = nestos_node::router(state, &[]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();
    let url = format!("http://{address}/v1/fs/docs/content?path=note.txt");
    fs::write(f.root.path().join("shared/note.txt"), "content").unwrap();
    assert_eq!(client.get(&url).send().await.unwrap().status(), 200);
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(READER)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .put(&url)
            .header("if-none-match", "*")
            .header("x-operation-id", "denied-put")
            .bearer_auth(READER)
            .body("changed")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .put(&url)
            .header("if-none-match", "*")
            .header("x-operation-id", "denied-put")
            .body("changed")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        fs::read_to_string(f.root.path().join("shared/note.txt")).unwrap(),
        "content"
    );
    let revision = client
        .get(&url)
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .headers()["etag"]
        .clone();
    let written = client
        .put(&url)
        .bearer_auth(OWNER)
        .header("if-match", revision)
        .header("x-operation-id", "owner-write")
        .body("owner change")
        .send()
        .await
        .unwrap();
    assert_eq!(written.status(), 200);
    assert_eq!(
        fs::read_to_string(f.root.path().join("shared/note.txt")).unwrap(),
        "owner change"
    );
    assert_eq!(f.ok(&["check", "owner", "docs", "write"])["allowed"], true);
    task.abort();
    let _ = task.await;
}

#[test]
fn inline_grants_stay_with_their_export_when_another_share_is_added() {
    let f = Fixture::new();
    f.ok(&["init"]);
    assert!(f.text().contains("grants = ["));
    assert!(!f.text().contains("[[exports.grants]]"));
    let second = f.root.path().join("second");
    fs::create_dir(&second).unwrap();
    f.ok(&["share", "add", "second", second.to_str().unwrap(), "rw"]);
    f.ok(&["grant", "owner", "second", "ro"]);
    let config: Config = toml::from_str(&f.text()).unwrap();
    let first = config
        .exports
        .iter()
        .find(|export| export.alias.as_deref() == Some("docs"))
        .unwrap();
    let other = config
        .exports
        .iter()
        .find(|export| export.alias.as_deref() == Some("second"))
        .unwrap();
    assert_eq!(first.grants.len(), 1);
    assert!(first.grants[0].execute);
    assert!(first.grants[0].access == nestos_node::config::model::Access::Rw);
    assert_eq!(other.grants.len(), 1);
    assert!(!other.grants[0].execute);
    assert!(!f.text().contains("[[exports.grants]]"));
    f.ok(&["revoke", "owner", "second"]);
    let config: Config = toml::from_str(&f.text()).unwrap();
    assert!(config.exports[1].grants.is_empty());
    assert_eq!(config.exports[0].grants.len(), 1);
}
