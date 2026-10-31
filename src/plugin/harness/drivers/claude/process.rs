//! One supervised bidirectional SDK process per owned native session.
use crate::{
    core::error::Error,
    harness::{config::ProfileConfig, events::Events},
    projects::runtime::ProjectRuntime,
};
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout},
    sync::{mpsc, oneshot, watch},
};
use tokio_util::sync::CancellationToken;
pub(super) type Reply = oneshot::Sender<Result<Value, Error>>;
struct Command {
    message: Value,
    key: Option<String>,
    reply: Reply,
}
pub struct Process {
    pub models: Arc<Mutex<Value>>,
    pub commands: Arc<Mutex<Value>>,
    sender: mpsc::Sender<Command>,
    pub session: Arc<Mutex<Value>>,
    pub items: Arc<Mutex<Vec<Value>>>,
    stop: CancellationToken,
    exited: watch::Receiver<bool>,
}
pub(super) struct Context {
    pub command: bool,
    pub command_output: bool,
    pub models: Arc<Mutex<Value>>,
    pub commands: Arc<Mutex<Value>>,
    pub id: String,
    pub turn: String,
    pub stream: String,
    pub interrupted: bool,
    pub steering: HashMap<String, String>,
    pub pending: HashMap<String, Reply>,
    pub requests: HashMap<String, Value>,
    pub events: Arc<Mutex<Events>>,
    pub session: Arc<Mutex<Value>>,
    pub(super) items: Arc<Mutex<Vec<Value>>>,
}
impl Context {
    fn new(
        id: String,
        events: Arc<Mutex<Events>>,
        session: Arc<Mutex<Value>>,
        items: Arc<Mutex<Vec<Value>>>,
        commands: Arc<Mutex<Value>>,
        models: Arc<Mutex<Value>>,
    ) -> Self {
        Self {
            command: false,
            command_output: false,
            models,
            commands,
            id,
            turn: String::new(),
            stream: String::new(),
            interrupted: false,
            steering: HashMap::new(),
            pending: HashMap::new(),
            requests: HashMap::new(),
            events,
            session,
            items,
        }
    }

    pub fn remember(&self, item: Value) {
        let mut items = self.items.lock().unwrap();
        if let Some(index) = items.iter().position(|prior| prior["id"] == item["id"]) {
            items[index] = item;
        } else {
            items.push(item);
        }
        while items.len() > 256
            || items.iter().map(|v| v.to_string().len()).sum::<usize>() > 2 * 1024 * 1024
        {
            items.remove(0);
        }
    }
}
impl Process {
    #[cfg(test)]
    pub(super) fn idle_fixture(id: &str) -> (Arc<Self>, watch::Sender<bool>) {
        let (sender, _) = mpsc::channel(1);
        let (exit, exited) = watch::channel(false);
        (
            Arc::new(Self {
                models: Arc::new(Mutex::new(json!({}))),
                commands: Arc::new(Mutex::new(json!([]))),
                sender,
                session: Arc::new(Mutex::new(json!({"id":id,"status":"idle","updatedAt":0}))),
                items: Arc::new(Mutex::new(Vec::new())),
                stop: CancellationToken::new(),
                exited,
            }),
            exit,
        )
    }
    pub async fn start(
        config: &ProfileConfig,
        runtime: Option<Arc<ProjectRuntime>>,
        session: Value,
        resume: Option<String>,
        events: Arc<Mutex<Events>>,
    ) -> Result<Arc<Self>, Error> {
        let child = launch(config, &runtime, &session, resume.as_deref())?;
        let process = Self::supervise(child, session, events, runtime)?;
        if let Err(error) = process.initialize().await {
            process.drain().await;
            return Err(error);
        }
        Ok(process)
    }
    async fn initialize(&self) -> Result<(), Error> {
        let value = self.control(json!({"subtype":"initialize"})).await?;
        *self.commands.lock().unwrap() = super::commands::catalog(&value["commands"]);
        if value["models"].is_array() {
            *self.models.lock().unwrap() = super::models::catalog(&value["models"])?;
        }
        Ok(())
    }
    fn supervise(
        mut child: Child,
        mut session: Value,
        events: Arc<Mutex<Events>>,
        runtime: Option<Arc<ProjectRuntime>>,
    ) -> Result<Arc<Self>, Error> {
        session["permissionMode"] = json!("manual");
        session["loaded"] = json!(true);
        let input = child.stdin.take().ok_or_else(Error::internal)?;
        let output = child.stdout.take().ok_or_else(Error::internal)?;
        let (sender, receiver) = mpsc::channel(128);
        let (exit, exited) = watch::channel(false);
        let id = session["id"]
            .as_str()
            .ok_or_else(Error::invalid)?
            .to_owned();
        let process = Arc::new(Self {
            models: Arc::new(Mutex::new(json!({"models":[]}))),
            commands: Arc::new(Mutex::new(json!([]))),
            sender,
            session: Arc::new(Mutex::new(session)),
            items: Arc::new(Mutex::new(Vec::new())),
            stop: CancellationToken::new(),
            exited,
        });
        let context = Context::new(
            id,
            events,
            process.session.clone(),
            process.items.clone(),
            process.commands.clone(),
            process.models.clone(),
        );
        let stop = process.stop.clone();
        tokio::spawn(async move {
            supervise(child, input, output, receiver, context, stop, runtime).await;
            let _ = exit.send(true);
        });
        Ok(process)
    }
    pub fn alive(&self) -> bool {
        !self.stop.is_cancelled()
    }
    pub fn exited(&self) -> bool {
        *self.exited.borrow()
    }
    pub fn close(&self) {
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
    pub async fn control(&self, request: Value) -> Result<Value, Error> {
        let id = super::uuid()?;
        self.send(
            json!({"type":"control_request","request_id":id,"request":request}),
            Some(id),
        )
        .await
    }
    pub async fn turn(
        &self,
        id: &str,
        content: Value,
        operation_id: Option<&str>,
    ) -> Result<Value, Error> {
        let session = self.session.lock().unwrap()["id"].clone();
        let started = std::time::Instant::now();
        // The shared mutation worker owns admission until native acknowledgement or process exit.
        // Its HTTP reply deadline must not discard the receiver for a later exact acknowledgement.
        let result = async {
            self.enqueue(json!({"type":"user","uuid":id,"session_id":session,"message":{"role":"user","content":content},"parent_tool_use_id":null}), Some(id.into()))
                .await?.await.map_err(|_| Error::unavailable())?
        }.await;
        admission_diagnostic(operation_id, &session, id, &result, started);
        result
    }
    pub async fn steer(
        &self,
        id: &str,
        target: &str,
        content: Value,
        operation: Option<&str>,
    ) -> Result<Value, Error> {
        let session = self.session.lock().unwrap()["id"].clone();
        let started = std::time::Instant::now();
        let result = self.enqueue(json!({"type":"user","uuid":id,"session_id":session,
            "message":{"role":"user","content":content},"parent_tool_use_id":null,"priority":"next","_steerTurnId":target}), Some(id.into()))
            .await?.await.map_err(|_|Error::unavailable())?;
        admission_diagnostic(operation, &session, target, &result, started);
        result
    }
    pub async fn respond(&self, id: &str, response: Value) -> Result<Value, Error> {
        self.send(
            json!({"type":"permission_reply","request_id":id,"response":response}),
            None,
        )
        .await
    }
    async fn send(&self, message: Value, key: Option<String>) -> Result<Value, Error> {
        let receive = self.enqueue(message, key).await?;
        tokio::time::timeout(Duration::from_secs(25), receive)
            .await
            .map_err(|_| Error::timed_out())?
            .map_err(|_| Error::unavailable())?
    }
    async fn enqueue(
        &self,
        message: Value,
        key: Option<String>,
    ) -> Result<oneshot::Receiver<Result<Value, Error>>, Error> {
        if !self.alive() {
            return Err(Error::unavailable());
        }
        let (reply, receive) = oneshot::channel();
        self.sender
            .try_send(Command {
                message,
                key,
                reply,
            })
            .map_err(|_| Error::unavailable())?;
        Ok(receive)
    }
}
fn admission_diagnostic(
    operation_id: Option<&str>,
    session: &Value,
    turn: &str,
    result: &Result<Value, Error>,
    started: std::time::Instant,
) {
    crate::core::events::emit(
        if result.is_ok() {
            crate::core::events::Level::Info
        } else {
            crate::core::events::Level::Warn
        },
        "harness.claude.admission",
        json!({"operation":"harness.turn.admission","operationId":operation_id.filter(|id|crate::core::ids::is_identifier_within(id,128)),
            "sessionId":session,"turnId":turn,"stage":"native_acknowledgement",
            "outcome":if result.is_ok(){"accepted"}else{"unknown"},"code":result.as_ref().err().map(|error|error.code),
            "durationMs":started.elapsed().as_millis()}),
    );
}

fn arguments(id: &str, resume: Option<&str>) -> Vec<String> {
    let mut args = [
        "--print",
        "--verbose",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--permission-prompt-tool",
        "stdio",
        "--replay-user-messages",
        "--include-partial-messages",
        "--permission-mode",
        "manual",
    ]
    .map(str::to_owned)
    .to_vec();
    args.extend([
        if resume.is_some() {
            "--resume"
        } else {
            "--session-id"
        }
        .into(),
        resume.unwrap_or(id).into(),
    ]);
    args
}
fn launch(
    config: &ProfileConfig,
    runtime: &Option<Arc<ProjectRuntime>>,
    session: &Value,
    resume: Option<&str>,
) -> Result<Child, Error> {
    let mut args = arguments(session["id"].as_str().ok_or_else(Error::invalid)?, resume);
    if session["catalogOnly"] == true {
        args.push("--no-session-persistence".into());
    }
    let mut prepared = runtime
        .as_ref()
        .map(|r| r.prepare(config, args.clone()))
        .transpose()?;
    let mut command = match prepared.as_mut() {
        Some(p) => std::mem::replace(&mut p.command, tokio::process::Command::new("/bin/false")),
        None => tokio::process::Command::new(&config.command),
    };
    if runtime.is_none() {
        command
            .args(args)
            .current_dir(session["cwd"].as_str().ok_or_else(Error::invalid)?);
    }
    environment(&mut command, config, runtime.is_some());
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Error::unavailable())
}
const ENVIRONMENT: &[&str] = &[
    "PATH",
    "LANG",
    "USER",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
];
fn environment(command: &mut tokio::process::Command, config: &ProfileConfig, scoped: bool) {
    command.env_clear();
    command.as_std_mut().process_group(0);
    for key in ENVIRONMENT {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    crate::harness::environment::inherit(command);
    command
        .env("CLAUDE_CONFIG_DIR", &config.home)
        .env("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    command.env(
        "HOME",
        if scoped {
            std::path::Path::new("/tmp")
        } else {
            config.home.as_path()
        },
    );
}
async fn supervise(
    mut child: Child,
    mut input: ChildStdin,
    mut output: ChildStdout,
    mut receiver: mpsc::Receiver<Command>,
    mut context: Context,
    stop: CancellationToken,
    runtime: Option<Arc<ProjectRuntime>>,
) {
    let mut line = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        tokio::select! {
            _=stop.cancelled()=>break,
            command=receiver.recv()=>{let Some(command)=command else{break;};if send_until_stopped(command,&mut input,&mut context,&stop).await.is_err(){break;}},
            read=output.read(&mut chunk)=>{let Ok(n)=read else{break;};if n==0 || consume(&chunk[..n],&mut line,&mut context).is_err(){break;}},
        }
    }
    cleanup(&mut child, &mut context, &stop, runtime).await;
}
async fn cleanup(
    child: &mut Child,
    context: &mut Context,
    stop: &CancellationToken,
    runtime: Option<Arc<ProjectRuntime>>,
) {
    if let Some(pid) = child
        .id()
        .and_then(|id| rustix::process::Pid::from_raw(id as i32))
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
    let clean = child.wait().await.is_ok();
    if let Some(runtime) = runtime {
        runtime.stopped_thread(&context.id, clean);
    }
    stop.cancel();
    for (_, reply) in context.pending.drain() {
        let _ = reply.send(Err(Error::unavailable()));
    }
    retire(context);
}

fn retire(context: &mut Context) {
    let requests = context.requests.keys().cloned().collect::<Vec<_>>();
    for id in requests {
        let _ = context
            .events
            .lock()
            .unwrap()
            .push(json!({"method":"serverRequest/resolved","params":{"requestId":id}}));
    }
    context.requests.clear();
    context.session.lock().unwrap()["owned"] = json!(false);
    context.session.lock().unwrap()["status"] = json!("notLoaded");
    let _ = context
        .events
        .lock()
        .unwrap()
        .push(json!({"method":"harness/disconnected","params":{"threadId":context.id}}));
}

async fn send_until_stopped(
    command: Command,
    input: &mut ChildStdin,
    context: &mut Context,
    stop: &CancellationToken,
) -> Result<(), Error> {
    tokio::select! {
        _ = stop.cancelled() => Err(Error::cancelled()),
        result = tokio::time::timeout(Duration::from_secs(25), send_command(command, input, context)) => result.map_err(|_| Error::timed_out())?,
    }
}

async fn send_command(
    command: Command,
    input: &mut ChildStdin,
    context: &mut Context,
) -> Result<(), Error> {
    let mut message = command.message;
    if message["type"] == "permission_reply" {
        message = match permission_reply(context, &message) {
            Ok(message) => message,
            Err(error) => {
                let _ = command.reply.send(Err(error));
                return Ok(());
            }
        };
    }
    let target = message
        .as_object_mut()
        .and_then(|row| row.remove("_steerTurnId"));
    if let Some(target) = target {
        if target.as_str() != Some(context.turn.as_str())
            || context.session.lock().unwrap()["status"] != "active"
        {
            let _ = command.reply.send(Err(Error::busy()));
            return Ok(());
        }
        if context.steering.len() >= 64 {
            let _ = command
                .reply
                .send(Err(Error::too_many("CLAUDE_STEERING_BUSY")));
            return Ok(());
        }
        context.steering.insert(
            message["uuid"].as_str().ok_or_else(Error::invalid)?.into(),
            target.as_str().unwrap().into(),
        );
    } else {
        mark_sent(context, &message)?;
    }
    let mut bytes = serde_json::to_vec(&message).map_err(|_| Error::internal())?;
    bytes.push(b'\n');
    input
        .write_all(&bytes)
        .await
        .map_err(|_| Error::unavailable())?;
    if let Some(key) = command.key {
        context.pending.insert(key, command.reply);
    } else {
        let _ = command.reply.send(Ok(json!({})));
    }
    Ok(())
}
fn mark_sent(context: &mut Context, message: &Value) -> Result<(), Error> {
    if message["type"] == "user" {
        context.command = message["message"]["content"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item["text"].as_str())
            .is_some_and(|text| text.trim_start().starts_with('/'));
        context.command_output = false;
        context.turn = message["uuid"].as_str().ok_or_else(Error::invalid)?.into();
        context.interrupted = false;
        context.session.lock().unwrap()["status"] = json!("active");
        context.session.lock().unwrap()["activeTurnId"] = json!(context.turn);
        context.events.lock().unwrap().push(json!({"method":"turn/started","params":{"threadId":context.id,"turn":{"id":context.turn}}}))?;
    }
    if message["request"]["subtype"] == "interrupt"
        || message["response"]["response"]["interrupt"] == true
    {
        context.interrupted = true;
    }
    Ok(())
}

fn consume(bytes: &[u8], line: &mut Vec<u8>, context: &mut Context) -> Result<(), Error> {
    for byte in bytes {
        if *byte == b'\n' {
            let frame = serde_json::from_slice::<Value>(line).map_err(|_| Error::invalid())?;
            line.clear();
            if frame["type"] == "control_response" {
                let id = frame["response"]["request_id"]
                    .as_str()
                    .ok_or_else(Error::invalid)?;
                if let Some(reply) = context.pending.remove(id) {
                    let result = if frame["response"]["subtype"] == "success" {
                        Ok(frame["response"]["response"].clone())
                    } else {
                        Err(Error::invalid())
                    };
                    let _ = reply.send(result);
                }
            } else {
                super::protocol::consume(context, frame)?;
            }
        } else {
            if line.len() >= 8 * 1024 * 1024 {
                return Err(Error::too_large("EFBIG"));
            }
            line.push(*byte);
        }
    }
    Ok(())
}
fn permission_reply(context: &mut Context, message: &Value) -> Result<Value, Error> {
    let id = message["request_id"].as_str().ok_or_else(Error::invalid)?;
    let request = context.requests.get(id).ok_or_else(Error::invalid)?;
    let response = &message["response"];
    let question = request["request"]["tool_name"] == "AskUserQuestion";
    let decision = response["decision"].as_str().unwrap_or("");
    let result = if question {
        question_reply(&request["request"]["input"], response)?
    } else if decision == "accept" {
        json!({"behavior":"allow","updatedInput":request["request"]["input"]})
    } else if decision == "decline" || decision == "cancel" {
        json!({"behavior":"deny","message":"Denied by user","interrupt":decision=="cancel"})
    } else {
        return Err(Error::invalid());
    };
    context.requests.remove(id);
    context
        .events
        .lock()
        .unwrap()
        .push(json!({"method":"serverRequest/resolved","params":{"requestId":id}}))?;
    Ok(
        json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":result}}),
    )
}

fn question_reply(original: &Value, response: &Value) -> Result<Value, Error> {
    let answers = response["answers"].as_object().ok_or_else(Error::invalid)?;
    let mut input = original.clone();
    let questions = input["questions"].as_array().ok_or_else(Error::invalid)?;
    let mut mapped = serde_json::Map::new();
    for (index, question) in questions.iter().enumerate() {
        let values = answers
            .get(&index.to_string())
            .and_then(|v| v["answers"].as_array())
            .ok_or_else(Error::invalid)?;
        if values.is_empty()
            || !values.iter().all(Value::is_string)
            || question["multiSelect"] != true && values.len() != 1
        {
            return Err(Error::invalid());
        }
        mapped.insert(
            question["question"]
                .as_str()
                .ok_or_else(Error::invalid)?
                .into(),
            json!(values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")),
        );
    }
    input["answers"] = Value::Object(mapped);
    Ok(json!({"behavior":"allow","updatedInput":input}))
}

#[cfg(test)]
mod interaction_tests {
    use super::*;
    #[test]
    fn converts_multiple_choices_without_accepting_multiple_answers_for_a_single_choice() {
        let mut input = json!({"questions":[{"question":"Features","multiSelect":true}]});
        let response = json!({"answers":{"0":{"answers":["A","B"]}}});
        assert_eq!(
            question_reply(&input, &response).unwrap()["updatedInput"]["answers"]["Features"],
            "A, B"
        );
        input["questions"][0]["multiSelect"] = json!(false);
        assert!(question_reply(&input, &response).is_err());
    }
}
