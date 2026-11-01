//! Supervised stdio JSON-RPC. The child owns Codex state; we never edit its database.
use super::{config::ProfileConfig, events::Events};
use crate::core::error::Error;
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, ChildStdout},
    sync::{mpsc, oneshot, watch},
};
use tokio_util::sync::CancellationToken;

type Reply = oneshot::Sender<Result<Value, Error>>;
type PendingCalls = Arc<Mutex<HashMap<u64, Pending>>>;
struct Pending {
    reply: Reply,
    method: String,
    started: Instant,
    context: Value,
}
enum Command {
    Call(String, Value, Reply, Value),
    Respond(Value, Value, Reply),
}
pub struct Bridge {
    sender: mpsc::Sender<Command>,
    pub events: Arc<Mutex<Events>>,
    stop: CancellationToken,
    exited: watch::Receiver<bool>,
    context: Value,
}

impl Bridge {
    pub async fn start(config: &ProfileConfig) -> Result<Arc<Self>, Error> {
        Self::start_scoped(config, None).await
    }
    pub async fn start_scoped(
        config: &ProfileConfig,
        runtime: Option<Arc<crate::projects::runtime::ProjectRuntime>>,
    ) -> Result<Arc<Self>, Error> {
        let mut prepared = runtime
            .as_ref()
            .map(|r| r.prepare(config, vec!["app-server".into()]))
            .transpose()?;
        let mut command = match prepared.as_mut() {
            Some(p) => {
                std::mem::replace(&mut p.command, tokio::process::Command::new("/bin/false"))
            }
            None => tokio::process::Command::new(&config.command),
        };
        command.env_clear();
        command.as_std_mut().process_group(0);
        for key in [
            "PATH",
            "HOME",
            "USER",
            "LANG",
            "TMPDIR",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        super::environment::inherit(&mut command);
        if runtime.is_none() {
            command.arg("app-server");
        }
        if let Some(runtime) = &runtime {
            command
                .env("HOME", "/tmp")
                .env("CELLS_PROJECT", &runtime.project.id);
        }
        let mut child = command
            .env("CODEX_HOME", &config.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| Error::unavailable())?;
        let input = child.stdin.take().ok_or_else(Error::internal)?;
        let output = child.stdout.take().ok_or_else(Error::internal)?;
        let (sender, receiver) = mpsc::channel(128);
        let (exit, exited) = watch::channel(false);
        let context = json!({"profileId":config.id,"processId":std::process::id(),"childPid":child.id(),"runtimeStartedAt":chrono::Utc::now().timestamp_millis(),
            "projectId":runtime.as_ref().map(|r| &r.project.id),
            "ownerId":runtime.as_ref().map(|r| &r.project.owner)});
        let bridge = Arc::new(Self {
            sender,
            events: Arc::new(Mutex::new(Events::new())),
            stop: CancellationToken::new(),
            exited,
            context: context.clone(),
        });
        bridge.events.lock().unwrap().runtime = runtime.clone();
        let events = bridge.events.clone();
        let stop = bridge.stop.clone();
        tokio::spawn(async move {
            supervise(child, input, output, receiver, events, stop, context).await;
            let _ = exit.send(true);
        });
        if let Err(error) = bridge.call("initialize", json!({"clientInfo":{"name":"itookit-agent","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await {
            bridge.close(); return Err(error);
        }
        // Notifications have no reply, but are written in the same ordered channel.
        bridge
            .respond(Value::Null, json!({"method":"initialized","params":{}}))
            .await?;
        Ok(bridge)
    }
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        tokio::time::timeout(
            Duration::from_secs(25),
            self.persistent_call(method, params),
        )
        .await
        .map_err(|_| Error::timed_out())?
    }
    pub async fn persistent_call(&self, method: &str, params: Value) -> Result<Value, Error> {
        if self.stop.is_cancelled() {
            return Err(Error::unavailable());
        }
        let (send, receive) = oneshot::channel();
        let mut context = super::diagnostics::native_context(&params);
        context
            .as_object_mut()
            .unwrap()
            .extend(self.context.as_object().unwrap().clone());
        self.sender
            .try_send(Command::Call(method.into(), params, send, context))
            .map_err(|_| Error::unavailable())?;
        receive.await.map_err(|_| Error::unavailable())?
    }
    pub async fn respond(&self, id: Value, result: Value) -> Result<Value, Error> {
        let (send, receive) = oneshot::channel();
        self.sender
            .try_send(Command::Respond(id, result, send))
            .map_err(|_| Error::unavailable())?;
        await_reply(receive).await
    }
    pub(super) fn recovery_context(&self) -> Value {
        json!({"runtimeId":self.events.lock().unwrap().epoch,
            "processId":self.context["processId"],"childPid":self.context["childPid"],
            "runtimeStartedAt":self.context["runtimeStartedAt"]})
    }
    pub fn close(&self) {
        if !self.stop.is_cancelled() {
            let mut fields = self.context.clone();
            fields["stage"] = json!("stop-requested");
            crate::core::events::emit(
                crate::core::events::Level::Info,
                "harness.native.stopping",
                fields,
            );
        }
        self.stop.cancel();
    }
    pub async fn drain(&self) {
        self.close();
        let mut exited = self.exited.clone();
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while !*exited.borrow() {
                if exited.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
}

async fn await_reply(receive: oneshot::Receiver<Result<Value, Error>>) -> Result<Value, Error> {
    tokio::time::timeout(Duration::from_secs(25), receive)
        .await
        .map_err(|_| Error::timed_out())?
        .map_err(|_| Error::unavailable())?
}

async fn write(input: &mut (impl AsyncWrite + Unpin), message: &Value) -> Result<(), Error> {
    let mut bytes = serde_json::to_vec(message).map_err(|_| Error::internal())?;
    bytes.push(b'\n');
    input
        .write_all(&bytes)
        .await
        .map_err(|_| Error::unavailable())
}

async fn supervise(
    mut child: Child,
    input: tokio::process::ChildStdin,
    output: ChildStdout,
    receiver: mpsc::Receiver<Command>,
    events: Arc<Mutex<Events>>,
    stop: CancellationToken,
    context: Value,
) {
    let started = Instant::now();
    crate::core::events::emit(
        crate::core::events::Level::Info,
        "harness.native.started",
        context.clone(),
    );
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let reason = pump(input, output, receiver, &pending, &events, &stop).await;
    // Reclaim this supervisor's process group before reaping its leader.
    if let Some(pid) = child
        .id()
        .and_then(|id| rustix::process::Pid::from_raw(id as i32))
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
    let stopped_by_host = stop.is_cancelled();
    let status = child.wait().await;
    let clean = status.is_ok();
    let mut fields = context;
    fields["stage"] = json!("reaped");
    fields["result"] = json!(if clean { "success" } else { "failure" });
    fields["stopRequested"] = json!(stopped_by_host);
    fields["code"] = json!(reason);
    fields["reaped"] = json!(clean);
    fields["exitCode"] = json!(status.as_ref().ok().and_then(|s| s.code()));
    fields["durationMs"] = json!(started.elapsed().as_millis() as u64);
    crate::core::events::emit(
        crate::core::events::Level::Info,
        "harness.native.stopped",
        fields,
    );
    if let Some(runtime) = events.lock().unwrap().runtime.as_ref() {
        runtime.stopped(clean);
    }
    stop.cancel();
    for (_, reply) in pending.lock().unwrap().drain() {
        let _ = reply.reply.send(Err(Error::unavailable()));
    }
    let _ = events
        .lock()
        .unwrap()
        .push(json!({"method":"harness/disconnected","params":{}}));
}

/// Keep draining native output while a large submission waits for stdin capacity.
async fn pump(
    input: impl AsyncWrite + Unpin,
    mut output: impl AsyncRead + Unpin,
    receiver: mpsc::Receiver<Command>,
    pending: &PendingCalls,
    events: &Mutex<Events>,
    stop: &CancellationToken,
) -> &'static str {
    let writer = send_commands(input, receiver, pending);
    tokio::pin!(writer);
    let mut line = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        tokio::select! {
            _ = stop.cancelled() => return "HOST_SHUTDOWN",
            reason = &mut writer => return reason,
            read = output.read(&mut chunk) => {
                let Ok(n) = read else { return "NATIVE_STDOUT_READ_FAILED"; };
                if n == 0 { return "NATIVE_STDOUT_EOF"; }
                if consume(&chunk[..n], &mut line, pending, events).is_err() { return "NATIVE_FRAME_INVALID"; }
            }
        }
    }
}

async fn send_commands(
    mut input: impl AsyncWrite + Unpin,
    mut receiver: mpsc::Receiver<Command>,
    pending: &PendingCalls,
) -> &'static str {
    let mut next = 0u64;
    while let Some(command) = receiver.recv().await {
        let sent = tokio::time::timeout(
            Duration::from_secs(25),
            send_command(command, &mut input, pending, &mut next),
        )
        .await;
        match sent {
            Ok(Ok(())) => (),
            Ok(Err(_)) => return "NATIVE_STDIN_WRITE_FAILED",
            Err(_) => return "NATIVE_STDIN_WRITE_TIMEOUT",
        }
    }
    "NATIVE_COMMAND_CHANNEL_CLOSED"
}

fn consume(
    chunk: &[u8],
    line: &mut Vec<u8>,
    pending: &PendingCalls,
    events: &Mutex<Events>,
) -> Result<(), Error> {
    for byte in chunk {
        if *byte == b'\n' {
            let message = serde_json::from_slice::<Value>(line).map_err(|_| Error::internal())?;
            line.clear();
            dispatch(message, pending, events)?;
        } else {
            if line.len() >= 8 * 1024 * 1024 {
                return Err(Error::too_large("EFBIG"));
            }
            line.push(*byte);
        }
    }
    Ok(())
}

async fn send_command(
    command: Command,
    input: &mut (impl AsyncWrite + Unpin),
    pending: &PendingCalls,
    next: &mut u64,
) -> Result<(), Error> {
    match command {
        Command::Call(method, params, reply, mut context) => {
            *next += 1;
            context["nativeRpcId"] = json!(*next);
            let message = json!({"id":next,"method":method,"params":params});
            if register(pending, *next, method, reply, context) {
                write(input, &message).await?;
            }
        }
        Command::Respond(id, result, reply) => {
            let message = if id.is_null() {
                result
            } else {
                json!({"id":id,"result":result})
            };
            let result = write(input, &message).await;
            let _ = reply.send(result.map(|_| json!({})));
            result?;
        }
    }
    Ok(())
}

fn register(pending: &PendingCalls, id: u64, method: String, reply: Reply, context: Value) -> bool {
    let mut pending = pending.lock().unwrap();
    pending.retain(|_, reply| !reply.reply.is_closed());
    if pending.len() >= 128 {
        let _ = reply.send(Err(Error::too_many("HARNESS_CALL_LIMIT")));
        return false;
    }
    pending.insert(
        id,
        Pending {
            reply,
            method,
            started: Instant::now(),
            context,
        },
    );
    true
}

fn dispatch(message: Value, pending: &PendingCalls, events: &Mutex<Events>) -> Result<(), Error> {
    if message.get("method").is_some() {
        return events.lock().unwrap().push(message);
    }
    let reply = message["id"]
        .as_u64()
        .and_then(|id| pending.lock().unwrap().remove(&id));
    if let Some(reply) = reply {
        let result = if message.get("error").is_some() {
            let error = native_error(&message["error"]);
            crate::core::events::emit(
                crate::core::events::Level::Warn,
                "harness.native.failed",
                native_result_fields(
                    &reply,
                    json!({
                        "method": reply.method, "rpcCode": message["error"]["code"].as_i64(),
                        "code": error.code, "stage":"native-reply", "result":"failure",
                        "durationMs": reply.started.elapsed().as_millis() as u64
                    }),
                ),
            );
            Err(error)
        } else {
            let mut result = message["result"].clone();
            if matches!(
                reply.method.as_str(),
                "thread/start"
                    | "thread/resume"
                    | "thread/archive"
                    | "thread/unarchive"
                    | "thread/delete"
            ) {
                crate::core::events::emit(
                    crate::core::events::Level::Info,
                    "harness.native.completed",
                    native_result_fields(
                        &reply,
                        json!({"method":reply.method,"stage":"native-reply","result":"success",
                        "durationMs":reply.started.elapsed().as_millis() as u64}),
                    ),
                );
            }
            if reply.method == "thread/read" && result.is_object() {
                result["_cellsEventCursor"] = events.lock().unwrap().position();
            }
            Ok(result)
        };
        let _ = reply.reply.send(result);
    }
    Ok(())
}

fn native_result_fields(reply: &Pending, details: Value) -> Value {
    let mut fields = reply.context.clone();
    fields
        .as_object_mut()
        .unwrap()
        .extend(details.as_object().unwrap().clone());
    fields
}

fn native_error(error: &Value) -> Error {
    if error["message"]
        .as_str()
        .is_some_and(|message| message.contains("already has an active writer"))
    {
        return Error::conflict("HARNESS_SESSION_BUSY");
    }
    if error["code"] == -32602 {
        return Error::conflict("HARNESS_INVALID_PARAMS");
    }
    if error["code"] == -32601 {
        return Error::unsupported();
    }
    if error["message"]
        .as_str()
        .is_some_and(|s| s.contains("not materialized yet"))
    {
        return Error::conflict("HARNESS_HISTORY_PENDING");
    }
    Error::conflict("HARNESS_RPC_ERROR")
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[tokio::test]
    async fn rejected_resume_keeps_native_and_operation_identifiers_without_prompt_or_paths() {
        let args = json!({"profileId":"codex","requestId":"request-1","sessionId":"session-1","prompt":"private-prompt"});
        let context = super::super::diagnostics::scope(
            super::super::diagnostics::operation_context("harness_resume", &args),
            async {
                super::super::diagnostics::native_context(
                    &json!({"threadId":"session-1","cwd":"private-path"}),
                )
            },
        )
        .await;
        let pending: PendingCalls = Arc::default();
        let (reply, receive) = oneshot::channel();
        let mut next = 0;
        send_command(
            Command::Call(
                "thread/resume".into(),
                json!({"threadId":"session-1"}),
                reply,
                context,
            ),
            &mut tokio::io::sink(),
            &pending,
            &mut next,
        )
        .await
        .unwrap();
        let fields = native_result_fields(
            &pending.lock().unwrap()[&1],
            json!({"code":"HARNESS_SESSION_BUSY"}),
        );
        assert_eq!(fields["requestId"], "request-1");
        assert_eq!(fields["nativeRpcId"], 1);
        assert_eq!(fields["sessionId"], "session-1");
        assert!(!fields.to_string().contains("private-"));
        dispatch(json!({"id":1,"error":{"code":-32600,"message":"private-prompt already has an active writer"}}),
            &pending, &Mutex::new(Events::new())).unwrap();
        assert_eq!(
            receive.await.unwrap().unwrap_err().code,
            "HARNESS_SESSION_BUSY"
        );
    }

    async fn backpressure_peer(
        mut input: tokio::io::DuplexStream,
        mut output: tokio::io::DuplexStream,
        sender: mpsc::Sender<Command>,
        prior: oneshot::Receiver<Result<Value, Error>>,
        pending: &PendingCalls,
        events: &Mutex<Events>,
        stop: &CancellationToken,
    ) {
        let (reply, mut receive) = oneshot::channel();
        sender
            .send(Command::Call(
                "turn/start".into(),
                json!({"input":"x".repeat(65536)}),
                reply,
                json!({}),
            ))
            .await
            .unwrap();
        // A byte proves the writer entered write_all; the peer then stops reading.
        input.read_u8().await.unwrap();
        write(&mut output, &json!({"method":"item/agentMessage/delta","params":{"threadId":"other","itemId":"item","delta":"y".repeat(4096)}})).await.unwrap();
        write(&mut output, &json!({"id":99,"result":{"ok":true}}))
            .await
            .unwrap();
        assert_eq!(prior.await.unwrap().unwrap()["ok"], true);
        assert_eq!(
            events.lock().unwrap().page(None, 0)["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(pending.lock().unwrap().contains_key(&1));
        assert!(receive.try_recv().is_err());
        stop.cancel();
    }
    #[tokio::test]
    async fn blocked_submission_keeps_draining_other_sessions_events_and_replies() {
        let (input, peer_input) = tokio::io::duplex(64);
        let (peer_output, output) = tokio::io::duplex(64);
        let (sender, receiver) = mpsc::channel(128);
        let pending: PendingCalls = Arc::default();
        let (reply, prior) = oneshot::channel();
        register(&pending, 99, "thread/read".into(), reply, json!({}));
        let events = Mutex::new(Events::new());
        let stop = CancellationToken::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                pump(input, output, receiver, &pending, &events, &stop),
                backpressure_peer(
                    peer_input,
                    peer_output,
                    sender,
                    prior,
                    &pending,
                    &events,
                    &stop
                )
            );
        })
        .await
        .expect("stdin backpressure must not stop stdout or unrelated replies");
    }

    #[test]
    fn retains_safe_native_error_categories() {
        assert_eq!(
            native_error(
                &json!({"code":-32600,"message":"thread private-id already has an active writer"})
            )
            .code,
            "HARNESS_SESSION_BUSY"
        );
        assert_eq!(
            native_error(&json!({"code":-32602,"message":"private details"})).code,
            "HARNESS_INVALID_PARAMS"
        );
        assert_eq!(native_error(&json!({"code":-32601})).code, "ECAPABILITY");
        assert_eq!(
            native_error(&json!({"code":-32000,"message":"thread not materialized yet"})).code,
            "HARNESS_HISTORY_PENDING"
        );
        assert_eq!(
            native_error(&json!({"code":-32000,"message":"private details"})).code,
            "HARNESS_RPC_ERROR"
        );
    }
}
