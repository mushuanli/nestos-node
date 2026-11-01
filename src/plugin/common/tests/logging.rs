//! Exercise the executable's default log sink without touching an installed server.
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn launch(path: &Path) -> Server {
    Server(
        Command::new(env!("CARGO_BIN_EXE_cells"))
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}
fn records(directory: &Path) -> Vec<Value> {
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .flat_map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect::<Vec<_>>()
        })
        .collect()
}
fn ready(directory: &Path, server: &mut Server) -> Value {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(record) = records(directory)
            .into_iter()
            .find(|r| r["event"] == "server.ready")
        {
            return record;
        }
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "server exited before ready"
        );
        assert!(Instant::now() < end, "server readiness timeout");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn server_records_startup_and_http_failure_without_authentication_secrets() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    let export = root.path().join("files");
    fs::create_dir(&export).unwrap();
    fs::write(
        &path,
        format!("listen='127.0.0.1:0'\nexecution=false\ntoken='test-private-token-at-least-24-bytes'\n[[exports]]\npath='{}'\n", export.display()),
    )
    .unwrap();
    let mut server = launch(&path);
    let log_dir = root.path().join("logs");
    let record = ready(&log_dir, &mut server);
    assert_eq!(record["fields"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(Path::new(record["fields"]["logFile"].as_str().unwrap()).is_file());
    let mut socket = TcpStream::connect(record["fields"]["address"].as_str().unwrap()).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket
        .write_all(b"GET /v1/capabilities HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 401"));
    let logs = records(&log_dir);
    assert_eq!(logs[0]["event"], "server.starting");
    assert_eq!(logs[0]["fields"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(logs
        .iter()
        .any(|r| r["event"] == "http.failed" && r["fields"]["status"] == 401));
    assert!(!serde_json::to_string(&logs)
        .unwrap()
        .contains("test-private-token"));
}

#[test]
fn startup_failure_after_config_loading_is_recorded() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    fs::write(&path, "listen='127.0.0.1:0'\nexecution=false\n").unwrap();
    let mut server = launch(&path);
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(Instant::now() < end, "server exit timeout");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(records(&root.path().join("logs"))
        .iter()
        .any(|r| r["event"] == "server.start_failed"
            && r["fields"]["version"] == env!("CARGO_PKG_VERSION")));
}

#[test]
fn version_query_needs_no_config_and_startup_banner_survives_failure() {
    let root = tempfile::tempdir().unwrap();
    for flag in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_cells"))
            .arg(flag)
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!("cells {}", env!("CARGO_PKG_VERSION"))
        );
        assert!(!root.path().join("logs").exists());
    }
    let output = Command::new(env!("CARGO_BIN_EXE_cells"))
        .arg(root.path().join("missing.toml"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with(&format!("cells {}\n", env!("CARGO_PKG_VERSION"))));
}
