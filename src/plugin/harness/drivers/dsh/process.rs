//! Launch only the supported web profile and supervise the process group to quiescence.
use super::{config::DshConfig, transport::Client};
use crate::{
    core::error::Error, harness::config::ProfileConfig, projects::runtime::ProjectRuntime,
};
use reqwest::Url;
use std::{os::unix::process::CommandExt, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Child,
    sync::{oneshot, watch},
};
use tokio_util::sync::CancellationToken;

pub struct Connection {
    pub client: Arc<Client>,
    exited: Option<watch::Receiver<bool>>,
}
struct Resources {
    _sandbox: Option<crate::process::sandbox::Prepared>,
    _overlay: tempfile::NamedTempFile,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.client.stop.cancel();
    }
}
impl Connection {
    pub async fn start(
        profile: &ProfileConfig,
        runtime: Option<Arc<ProjectRuntime>>,
        stop: CancellationToken,
    ) -> Result<Self, Error> {
        let options = profile.dsh.clone().unwrap_or_default();
        let (url, token, exited) = if let Some(url) = &options.url {
            let token = options
                .token_env
                .as_ref()
                .map(|key| std::env::var(key).map_err(|_| Error::unauthenticated()))
                .transpose()?;
            (
                super::config::origin(url).map_err(|_| Error::invalid())?,
                token,
                None,
            )
        } else {
            let (url, token, exited) = launch(profile, runtime, &options, stop.clone()).await?;
            (url, Some(token), Some(exited))
        };
        match Client::connect(url, token, &options, stop.clone()).await {
            Ok(client) => Ok(Self { client, exited }),
            Err(error) => {
                stop.cancel();
                Err(error)
            }
        }
    }
    pub async fn drain(&self) {
        self.client.stop.cancel();
        if let Some(mut exited) = self.exited.clone() {
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                while !*exited.borrow() && exited.changed().await.is_ok() {}
            })
            .await;
        }
    }
}

async fn launch(
    profile: &ProfileConfig,
    runtime: Option<Arc<ProjectRuntime>>,
    options: &DshConfig,
    stop: CancellationToken,
) -> Result<(Url, String, watch::Receiver<bool>), Error> {
    let (mut command, prepared) = prepare(profile, runtime.as_deref())?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Error::unavailable())?;
    let output = child.stdout.take().ok_or_else(Error::internal)?;
    let (ready, receive) = oneshot::channel();
    let exited = spawn_supervisor(child, output, ready, runtime, prepared, stop.clone());
    match tokio::time::timeout(Duration::from_millis(options.startup_timeout_ms), receive).await {
        Ok(Ok((url, token))) => Ok((url, token, exited)),
        _ => {
            stop.cancel();
            Err(Error::new(
                axum::http::StatusCode::GATEWAY_TIMEOUT,
                "DSH_STARTUP_FAILED",
            ))
        }
    }
}
fn prepare(
    profile: &ProfileConfig,
    runtime: Option<&ProjectRuntime>,
) -> Result<(tokio::process::Command, Resources), Error> {
    let overlay = overlay(profile)?;
    let mut args = vec![
        "--patch".into(),
        overlay.path().to_str().ok_or_else(Error::invalid)?.into(),
    ];
    args.extend(arguments());
    let mut prepared = runtime
        .map(|r| r.prepare_with_source_paths(profile, args.clone()))
        .transpose()?;
    let mut command = match prepared.as_mut() {
        Some(p) => std::mem::replace(&mut p.command, tokio::process::Command::new("/bin/false")),
        None => {
            let mut command = tokio::process::Command::new(&profile.command);
            command.args(args);
            command
        }
    };
    environment(&mut command, profile, runtime);
    Ok((
        command,
        Resources {
            _sandbox: prepared,
            _overlay: overlay,
        },
    ))
}
fn environment(
    command: &mut tokio::process::Command,
    profile: &ProfileConfig,
    runtime: Option<&ProjectRuntime>,
) {
    command.env_clear();
    command.as_std_mut().process_group(0);
    for key in [
        "PATH",
        "HOME",
        "USER",
        "LANG",
        "DEEPSEEK_API_KEY",
        "DEEPSEEK_BASE_URL",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    super::super::environment::inherit(command);
    command.env("DSH_HOME", &profile.home);
    if let Some(runtime) = runtime {
        command
            .env("HOME", "/tmp")
            .env("CELLS_PROJECT", &runtime.project.id);
    } else if let Some(workspace) = profile.workspaces.first() {
        command.current_dir(&workspace.path);
    }
}
fn spawn_supervisor(
    mut child: Child,
    output: tokio::process::ChildStdout,
    ready: oneshot::Sender<(Url, String)>,
    runtime: Option<Arc<ProjectRuntime>>,
    prepared: Resources,
    stop: CancellationToken,
) -> watch::Receiver<bool> {
    let (exit, exited) = watch::channel(false);
    tokio::spawn(async move {
        let clean = supervise(&mut child, output, ready, &stop).await;
        stop.cancel();
        if let Some(runtime) = runtime {
            runtime.stopped(clean);
        }
        drop(prepared);
        let _ = exit.send(true);
    });
    exited
}

async fn supervise(
    child: &mut Child,
    output: tokio::process::ChildStdout,
    ready: oneshot::Sender<(Url, String)>,
    stop: &CancellationToken,
) -> bool {
    let mut ready = Some(ready);
    let mut reader = BufReader::new(output);
    let mut eof = false;
    loop {
        let mut line = Vec::new();
        let mut limited = (&mut reader).take(16_384);
        tokio::select! {
            _ = stop.cancelled() => break,
            result = child.wait() => return result.is_ok_and(|status| status.success()),
            result = limited.read_until(b'\n', &mut line), if !eof => {
                if !matches!(result, Ok(n) if n > 0) { eof = true; continue; }
                if let Some(endpoint) = startup_url(&line) {
                    if let Some(ready) = ready.take() { let _ = ready.send(endpoint); }
                }
            }
        }
    }
    reap(child).await;
    false
}
async fn reap(child: &mut Child) {
    if let Some(pid) = child
        .id()
        .and_then(|pid| rustix::process::Pid::from_raw(pid as i32))
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn startup_url(line: &[u8]) -> Option<(Url, String)> {
    let text = std::str::from_utf8(line).ok()?;
    let raw = text
        .trim()
        .strip_prefix("dsh web: ")?
        .split_whitespace()
        .next()?;
    let mut url = Url::parse(raw).ok()?;
    if url.scheme() != "http" || url.host_str() != Some("127.0.0.1") || url.path() != "/" {
        return None;
    }
    let token = url
        .query_pairs()
        .find(|(key, _)| key == "token")?
        .1
        .into_owned();
    url.set_query(None);
    if token.is_empty() {
        return None;
    }
    Some((url, token))
}

fn arguments() -> Vec<String> {
    [
        "--profile",
        "web",
        "--no-open",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
fn overlay(profile: &ProfileConfig) -> Result<tempfile::NamedTempFile, Error> {
    use std::io::Write;
    let mut file = tempfile::Builder::new()
        .prefix(".cells-transport-")
        .suffix(".yml")
        .tempfile_in(&profile.home)?;
    file.write_all(b"- id: webserver\n  config:\n    host: 127.0.0.1\n    port: 0\n- id: web-runtime\n  config:\n    openBrowser: false\n    printUrl: true\n")?;
    file.flush()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_overlay_is_private_and_removed_after_the_child_lifetime() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let profile: ProfileConfig = toml::from_str(&format!(
            "id='dsh'\nkind='dsh'\ncommand='dsh'\nhome={:?}",
            home.path()
        ))
        .unwrap();
        let file = overlay(&profile).unwrap();
        let path = file.path().to_owned();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("host: 127.0.0.1\n    port: 0"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(file);
        assert!(!path.exists());
    }
}
