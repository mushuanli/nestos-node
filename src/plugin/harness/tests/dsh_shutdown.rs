//! A service-manager stop must retire the managed DSH process group.
#[allow(dead_code)]
#[path = "support/dsh.rs"]
mod fixture;
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, process::Stdio, time::Duration};

#[tokio::test]
async fn sigterm_drains_managed_dsh_before_the_node_exits() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let pid_file = root.path().join("native.pid");
    let command = root.path().join("dsh");
    std::fs::write(&command,format!("#!/bin/sh\nprintf '%s' \"$$\" > {pid_file:?}\nprintf 'dsh web: {}?token=fixture\\n'\nexec sleep 300\n",host.url)).unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let config = root.path().join("config.toml");
    let home = root.path();
    std::fs::write(
        &config,
        format!(
            r#"listen="{address}"
execution=false
api_key="shutdown-fixture-token-0123456789"
[[harnesses]]
id="dsh"
kind="dsh"
command={command:?}
home={home:?}
[[harnesses.workspaces]]
id="project"
path={home:?}
"#
        ),
    )
    .unwrap();
    let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_cells"))
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let mut ready = false;
    for _ in 0..100 {
        if let Ok(response)=http.post(format!("http://{address}/mcp"))
            .bearer_auth("shutdown-fixture-token-0123456789")
            .header("mcp-protocol-version","2026-07-28").header("mcp-method","tools/call").header("mcp-name","harness_sessions")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"harness_sessions","arguments":{"profileId":"dsh"},"_meta":{
                "io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"shutdown-test","version":"1"}}}}))
            .send().await {
            if response.status().is_success() && pid_file.exists(){ready=true;break;}
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready, "managed child did not start");
    let native_pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let parent_pid = rustix::process::Pid::from_raw(process.id().unwrap() as i32).unwrap();
    rustix::process::kill_process(parent_pid, rustix::process::Signal::TERM).unwrap();
    let exited = tokio::time::timeout(Duration::from_secs(10), process.wait()).await;
    let orphan = std::path::Path::new(&format!("/proc/{native_pid}")).exists();
    if orphan {
        let _ = rustix::process::kill_process_group(
            rustix::process::Pid::from_raw(native_pid).unwrap(),
            rustix::process::Signal::KILL,
        );
    }
    assert!(
        exited.unwrap().unwrap().success(),
        "SIGTERM should use graceful shutdown"
    );
    assert!(!orphan, "DSH child must be reaped before node exit");
}
