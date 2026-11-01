use cells::{app::State, config::Config};
use std::sync::{Arc, Mutex, MutexGuard};

// Credential resolution reads the process environment, so the tests serialize.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    ENVIRONMENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn load(body: &str) -> Result<Arc<State>, String> {
    let text = format!("listen = \"127.0.0.1:0\"\n{body}");
    let config: Config = toml::from_str(&text).unwrap();
    State::from_config(&config)
}

/// A temporary directory plus a valid child directory to export.
fn fixture() -> (tempfile::TempDir, String) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("x1");
    std::fs::create_dir(&path).unwrap();
    (root, path.display().to_string())
}

#[test]
fn derives_alias_from_path_and_defaults_to_read_only() {
    let _guard = lock();
    let (_root, path) = fixture();
    let state = load(&format!(
        "username = \"li\"\npassword = \"12345678\"\n[[exports]]\npath = \"{path}\"\n"
    ))
    .unwrap();
    let client = &state.auth.clients()[0];
    assert_eq!(client.username(), Some("li"));
    assert_eq!(client.exports(), ["x1"]);
    assert!(!client.can_write());
    assert!(!state.exports.get("x1").unwrap().writable());
}

#[test]
fn read_write_access_implies_exclusive_writes() {
    let _guard = lock();
    let (_root, path) = fixture();
    let state = load(&format!(
        "username = \"li\"\npassword = \"12345678\"\n[[exports]]\npath = \"{path}\"\naccess = \"rw\"\n"
    ))
    .unwrap();
    assert!(state.auth.clients()[0].may_write("x1"));
    assert!(state.exports.get("x1").unwrap().writable());
    assert!(cells::fs::Export::exclusive(std::path::Path::new(&path)).is_err());
}

#[test]
fn explicit_alias_overrides_the_directory_name() {
    let _guard = lock();
    let (_root, path) = fixture();
    let state = load(&format!(
        "username = \"li\"\npassword = \"12345678\"\n[[exports]]\nalias = \"ds\"\npath = \"{path}\"\n"
    ))
    .unwrap();
    assert_eq!(state.auth.clients()[0].exports(), ["ds"]);
}

#[test]
fn token_clients_use_bearer_without_a_username() {
    let _guard = lock();
    let (_root, path) = fixture();
    let state = load(&format!(
        "token = \"token-at-least-24-bytes-long\"\n[[exports]]\npath = \"{path}\"\n"
    ))
    .unwrap();
    assert!(state.auth.clients()[0].username().is_none());
}

#[test]
fn api_key_configuration_authenticates_bearer_without_username() {
    let _guard = lock();
    let (_root, path) = fixture();
    let state = load(&format!(
        "api_key = 'api-key-at-least-24-bytes-long'\n[[exports]]\npath = '{path}'\n"
    ))
    .unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        "Bearer api-key-at-least-24-bytes-long".parse().unwrap(),
    );
    assert_eq!(state.auth.identify(&headers).unwrap(), 0);
    assert!(state.auth.client(0).username().is_none());
    headers.insert("authorization", "Bearer incorrect-api-key".parse().unwrap());
    assert!(state.auth.identify(&headers).is_err());
}

#[test]
fn api_key_and_legacy_token_cannot_both_be_configured() {
    assert!(toml::from_str::<Config>("listen='127.0.0.1:0'\napi_key='one'\ntoken='two'").is_err());
}

#[test]
fn token_clients_reject_a_username_that_would_look_like_basic_auth() {
    let _guard = lock();
    let (_root, path) = fixture();
    let error = load(&format!(
        "username = \"li\"\ntoken = \"token-at-least-24-bytes-long\"\n[[exports]]\npath = \"{path}\"\n"
    ))
    .err()
    .expect("a token plus username must be rejected");
    assert!(error.contains("username"), "{error}");
}

#[test]
fn environment_variants_still_resolve() {
    let _guard = lock();
    std::env::set_var("FS_SERVER_TEST_PASSWORD", "environment-password-12");
    let (_root, path) = fixture();
    let state = load(&format!(
        "username = \"dave\"\npassword_env = \"FS_SERVER_TEST_PASSWORD\"\n[[exports]]\npath = \"{path}\"\n"
    ))
    .unwrap();
    let client = &state.auth.clients()[0];
    assert_eq!(client.username(), Some("dave"));
    assert_eq!(client.secret(), "environment-password-12");
}

#[test]
fn rejects_short_or_conflicting_credentials() {
    let _guard = lock();
    let (_root, path) = fixture();
    for body in [
        format!("username = \"li\"\npassword = \"short\"\n[[exports]]\npath = \"{path}\"\n"),
        format!("username = \"li\"\npassword = \"12345678\"\ntoken = \"token-at-least-24-bytes-long\"\n[[exports]]\npath = \"{path}\"\n"),
        format!("username = \"li\"\npassword = \"12345678\"\npassword_env = \"FS_SERVER_PASSWORD\"\n[[exports]]\npath = \"{path}\"\n"),
        format!("username = \"li\"\n[[exports]]\npath = \"{path}\"\n"),
        "username = \"li\"\npassword = \"12345678\"\n".to_owned(),
    ] {
        assert!(load(&body).is_err(), "{body}");
    }
}

#[test]
fn reports_a_missing_username_and_environment_variable() {
    let _guard = lock();
    std::env::remove_var("FS_SERVER_USER");
    std::env::remove_var("FS_SERVER_TEST_UNSET_PASSWORD");
    let (_root, path) = fixture();
    let missing_username = load(&format!(
        "password = \"12345678\"\n[[exports]]\npath = \"{path}\"\n"
    ))
    .err()
    .expect("a missing username must be rejected");
    assert!(
        missing_username.contains("username is required"),
        "{missing_username}"
    );
    let missing_variable = load(&format!(
        "username = \"li\"\npassword_env = \"FS_SERVER_TEST_UNSET_PASSWORD\"\n[[exports]]\npath = \"{path}\"\n"
    ))
    .err()
    .expect("a missing environment variable must be rejected");
    assert!(
        missing_variable.contains("FS_SERVER_TEST_UNSET_PASSWORD"),
        "{missing_variable}"
    );
}

#[test]
fn rejects_invalid_duplicate_and_overlapping_exports() {
    let _guard = lock();
    let root = tempfile::tempdir().unwrap();
    let spaced = root.path().join("my dir");
    let outer = root.path().join("a");
    let inner = outer.join("b");
    for path in [&spaced, &outer, &inner] {
        std::fs::create_dir(path).unwrap();
    }
    let base = "username = \"li\"\npassword = \"12345678\"\n";
    for (body, expected) in [
        (
            format!("{base}[[exports]]\npath = \"{}\"\n", spaced.display()),
            "set \"alias\"",
        ),
        (
            format!(
                "{base}[[exports]]\nalias = \"same\"\npath = \"{}\"\n[[exports]]\nalias = \"same\"\npath = \"{}\"\n",
                spaced.display(),
                inner.display()
            ),
            "already defined",
        ),
        (
            format!(
                "{base}[[exports]]\npath = \"{}\"\n[[exports]]\npath = \"{}\"\n",
                outer.display(),
                inner.display()
            ),
            "overlaps",
        ),
    ] {
        let error = load(&body).err().expect("the export must be rejected");
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn rejects_an_unknown_access_value() {
    let parsed: Result<Config, _> = toml::from_str(
        "listen = \"127.0.0.1:0\"\nusername = \"li\"\npassword = \"12345678\"\n[[exports]]\npath = \"/tmp\"\naccess = \"rx\"\n",
    );
    assert!(parsed.is_err());
}

#[test]
fn stable_node_identity_is_explicit_and_validated() {
    let _guard = lock();
    let (_root, path) = fixture();
    let config = |id: &str| {
        format!("server_id = {id:?}\nusername = \"li\"\npassword = \"12345678\"\n[[exports]]\npath = {path:?}\n")
    };
    assert_eq!(
        load(&config("my-node")).unwrap().auth.server_id(),
        Some("my-node")
    );
    assert!(load(&config("../other-node")).is_err());
    assert!(load(&config("")).is_err());
}

#[test]
fn execution_defaults_on_with_an_ephemeral_identity_and_can_be_disabled() {
    let _guard = lock();
    let (_root, path) = fixture();
    let base =
        format!("username = \"li\"\npassword = \"12345678\"\n[[exports]]\npath = {path:?}\n");
    let first = load(&base).unwrap();
    let second = load(&base).unwrap();
    assert!(first.auth.server_id().unwrap().starts_with("cells-"));
    assert_ne!(first.auth.server_id(), second.auth.server_id());
    assert_eq!(
        load(&format!("execution = false\n{base}"))
            .unwrap()
            .auth
            .server_id(),
        None
    );
}

/// The published process epoch must be independent randomness, never the key
/// that signs pagination cursors.
#[test]
fn process_epoch_is_independent_of_the_cursor_key() {
    let _guard = lock();
    let (_root, path) = fixture();
    let body =
        format!("username = \"li\"\npassword = \"12345678\"\n[[exports]]\npath = \"{path}\"\n");
    let first = load(&body).unwrap();
    let second = load(&body).unwrap();
    let key_hex: String = first
        .cursor_key
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_ne!(first.epoch.as_str(), key_hex);
    assert_ne!(first.epoch.as_str(), second.epoch.as_str());
}

/// A first start on a missing or empty sync root creates the store instead of
/// failing the launch, without touching the export layout.
#[test]
fn startup_initializes_a_missing_sync_root() {
    let _guard = lock();
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let state = load(&format!(
        "execution = false\nusername = \"li\"\npassword = \"12345678\"\n\
         [sync]\nenabled = true\nroot = {store:?}\nmetadata_reserve_bytes = 0\n"
    ))
    .unwrap();
    let sync = state
        .services
        .get::<cells::sync::SyncService>()
        .expect("sync must be served");
    assert!(store.join("storage.json").exists());
    assert!(store.join("metadata.db").exists());
    assert!(!store.join("init.pending").exists());
    assert_eq!(
        sync.capabilities()["authorityId"].as_str().unwrap().len(),
        64
    );
}

#[test]
fn startup_initializes_a_sync_root_holding_only_a_stale_lock_file() {
    let _guard = lock();
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    std::fs::create_dir(&store).unwrap();
    std::fs::write(store.join("sync.lock"), b"").unwrap();
    load(&format!(
        "execution = false\nusername = \"li\"\npassword = \"12345678\"\n\
         [sync]\nenabled = true\nroot = {store:?}\nmetadata_reserve_bytes = 0\n"
    ))
    .unwrap();
    assert!(store.join("metadata.db").exists());
}

/// The isolation check runs before the root exists, so it must resolve the path
/// that is about to be created rather than skipping it.
#[test]
fn startup_rejects_a_sync_root_that_would_be_created_inside_an_export() {
    let _guard = lock();
    let root = tempfile::tempdir().unwrap();
    let export = root.path().join("x1");
    std::fs::create_dir(&export).unwrap();
    let nested = export.join("store");
    let error = load(&format!(
        "execution = false\nusername = \"li\"\npassword = \"12345678\"\n\
         [[exports]]\npath = {export:?}\n\
         [sync]\nenabled = true\nroot = {nested:?}\n"
    ))
    .err()
    .expect("an overlapping root must be rejected");
    assert!(error.contains("SYNC_EXPORT_OVERLAP"), "{error}");
    assert!(!nested.exists());
}
