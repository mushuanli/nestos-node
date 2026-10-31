//! Claude Code's SDK transport and JSONL history, independent of Codex's app-server.
mod commands;
mod discovery;
mod history;
mod models;
mod process;
mod protocol;
mod search;
mod sessions;
mod system;
use crate::{
    core::error::Error,
    harness::{
        config::{ProfileConfig, WorkspaceConfig},
        driver::HarnessDriver,
        events::Events,
        service::string,
    },
    projects::runtime::ProjectRuntime,
};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
static READS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
pub struct Claude {
    discovery: tokio::sync::Mutex<BTreeMap<String, Value>>,
    config: ProfileConfig,
    runtime: Option<Arc<ProjectRuntime>>,
    sessions: sessions::Sessions,
    events: Arc<Mutex<Events>>,
    closed: AtomicBool,
    usage: Arc<super::usage::Reader>,
}
impl Claude {
    pub fn new(mut config: ProfileConfig, runtime: Option<Arc<ProjectRuntime>>) -> Self {
        if let Some(runtime) = &runtime {
            config.workspaces = vec![WorkspaceConfig {
                id: runtime.project.id.clone(),
                path: runtime.cwd().into(),
            }];
        }
        let mut events = Events::new();
        events.runtime = runtime.clone();
        Self {
            discovery: tokio::sync::Mutex::new(BTreeMap::new()),
            config,
            runtime,
            sessions: sessions::Sessions::default(),
            events: Arc::new(Mutex::new(events)),
            closed: AtomicBool::new(false),
            usage: Arc::default(),
        }
    }
    async fn catalog(&self) -> Result<history::Catalog, Error> {
        static WORK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
        let permit = WORK
            .try_acquire()
            .map_err(|_| Error::too_many("CLAUDE_HISTORY_BUSY"))?;
        let config = self.config.clone();
        let runtime = self.runtime.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            history::catalog(&config, runtime.as_deref())
        })
        .await
        .map_err(|_| Error::internal())?
    }
    async fn metrics(&self, args: &Value) -> Result<Value, Error> {
        let started = std::time::Instant::now();
        let id = string(args, "sessionId")?;
        let session = self.session(id).await?;
        let mut metrics = session
            .get("metrics")
            .cloned()
            .unwrap_or_else(super::metrics::empty);
        let Some(process) = self.sessions.get(id).await else {
            return Ok(json!({"sessionId":id,"metrics":metrics}));
        };
        match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            process.control(json!({"subtype":"get_context_usage"})),
        )
        .await
        {
            Ok(Ok(raw)) => {
                if let Some(value) = super::metrics::context(&raw["totalTokens"], &raw["maxTokens"])
                {
                    metrics["contextWindow"] = value;
                }
            }
            result => {
                metrics["errors"]["context"] = json!(match result {
                    Ok(Err(error)) => error.code,
                    _ => "ETIMEDOUT",
                });
            }
        }
        metrics["observedAt"] = json!(chrono::Utc::now().timestamp_millis());
        super::metrics::report(args, &metrics, started);
        Ok(json!({"sessionId":id,"metrics":metrics}))
    }
    async fn session(&self, id: &str) -> Result<Value, Error> {
        if !history::uuid(id) {
            return Err(Error::invalid());
        }
        if let Some(process) = self.sessions.get(id).await {
            return Ok(process.session.lock().unwrap().clone());
        }
        self.catalog()
            .await?
            .entries
            .into_iter()
            .find(|entry| entry.session["id"] == id)
            .map(|entry| entry.session)
            .ok_or_else(|| Error::not_found("ENOENT"))
    }
    async fn list(&self, args: &Value) -> Result<Value, Error> {
        if args["archived"].as_bool() == Some(true) {
            return Ok(json!({"sessions":[],"nextCursor":null}));
        }
        let mut sessions = self
            .catalog()
            .await?
            .entries
            .into_iter()
            .map(|entry| {
                (
                    entry.session["id"].as_str().unwrap().to_owned(),
                    entry.session,
                )
            })
            .collect::<BTreeMap<_, _>>();
        for process in self.sessions.live() {
            let session = process.session.lock().unwrap().clone();
            sessions.insert(session["id"].as_str().unwrap().into(), session);
        }
        list_page(sessions.into_values().collect(), args)
    }
    async fn read_history(&self, args: &Value) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        let mut session = self.session(id).await?;
        let entry = self
            .catalog()
            .await?
            .entries
            .into_iter()
            .find(|entry| entry.session["id"] == id);
        let mut result = if let Some(entry) = entry {
            self.history_page(entry, args.clone()).await?
        } else {
            let items = self.live_items(id).await;
            json!({"session":session,"turns":[{"id":null,"items":items}],"nextCursor":null})
        };
        if !args["cursor"].is_string() && args["digestProjection"] != true {
            super::usage::attach(&mut session, self.usage(args).await);
        }
        result["session"] = session;
        Ok(result)
    }
    async fn history_page(&self, entry: history::Entry, args: Value) -> Result<Value, Error> {
        let permit = READS
            .try_acquire()
            .map_err(|_| Error::too_many("CLAUDE_HISTORY_BUSY"))?;
        let runtime = self.runtime.clone();
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            history::read(&config, runtime.as_deref(), &entry, &args)
        })
        .await
        .map_err(|_| Error::internal())?
    }
    async fn usage(&self, args: &Value) -> Result<Value, Error> {
        let id = string(args, "sessionId")?.to_owned();
        self.session(&id).await?;
        let entry = self
            .catalog()
            .await?
            .entries
            .into_iter()
            .find(|e| e.session["id"] == id)
            .ok_or_else(|| Error::not_found("ENOENT"))?;
        let (config, runtime, reader) = (
            self.config.clone(),
            self.runtime.clone(),
            self.usage.clone(),
        );
        let usage_permit = super::usage::permit()?;
        let permit = READS
            .try_acquire()
            .map_err(|_| Error::too_many("CLAUDE_HISTORY_BUSY"))?;
        tokio::task::spawn_blocking(move || {
            let _usage_permit = usage_permit;
            let _permit = permit;
            let file = history::usage_file(&config, &entry)?;
            reader.read(file, &id, super::usage::Kind::Claude, |record| {
                history::authorize_record(&config, runtime.as_deref(), &entry, record)
            })
        })
        .await
        .map_err(|_| Error::internal())?
    }
    async fn live_items(&self, id: &str) -> Vec<Value> {
        let Some(process) = self.sessions.get(id).await else {
            return Vec::new();
        };
        let items = process.items.lock().unwrap();
        items
            .iter()
            .rev()
            .take(100)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
    async fn attach(&self, session: Value, resume: bool) -> Result<Value, Error> {
        let id = string(&session, "id")?.to_owned();
        self.check_read()?;
        let slot = self.sessions.slot(&id, &self.events).await?;
        let mut attached = slot.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::unavailable());
        }
        if let Some(process) = attached.as_ref() {
            if process.alive() {
                return Ok(json!({"session":process.session.lock().unwrap().clone()}));
            }
        }
        let resume = if resume {
            Some(self.resume_path(&id).await?)
        } else {
            None
        };
        let process = self.start(session, resume).await?;
        let session = process.session.lock().unwrap().clone();
        *attached = Some(process);
        Ok(json!({"session":session}))
    }
    async fn start(
        &self,
        session: Value,
        resume: Option<String>,
    ) -> Result<Arc<process::Process>, Error> {
        let mut session = session;
        session["owned"] = json!(true);
        session["status"] = json!("idle");
        session["resumable"] = json!(true);
        let process = process::Process::start(
            &self.config,
            self.runtime.clone(),
            session,
            resume,
            self.events.clone(),
        )
        .await?;
        if self.closed.load(Ordering::Acquire) {
            process.close();
            process.drain().await;
            return Err(Error::unavailable());
        }
        Ok(process)
    }
    async fn resume_path(&self, id: &str) -> Result<String, Error> {
        let entry = self
            .catalog()
            .await?
            .entries
            .into_iter()
            .find(|entry| entry.session["id"] == id)
            .ok_or_else(|| Error::not_found("ENOENT"))?;
        let config = self.config.clone();
        let runtime = self.runtime.clone();
        let permit = READS
            .try_acquire()
            .map_err(|_| Error::too_many("CLAUDE_HISTORY_BUSY"))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            history::resume_path(&config, runtime.as_deref(), &entry)
        })
        .await
        .map_err(|_| Error::internal())?
    }
    async fn create(&self, args: &Value) -> Result<Value, Error> {
        let workspace = self
            .config
            .workspaces
            .iter()
            .find(|w| Some(w.id.as_str()) == args["workspaceId"].as_str())
            .ok_or_else(Error::invalid)?;
        self.attach(json!({"id":uuid()?,"title":"","cwd":workspace.path,"status":"idle","createdAt":chrono::Utc::now().timestamp_millis(),"updatedAt":chrono::Utc::now().timestamp_millis(),"resumable":true,"owned":true,"archived":false}),false).await
    }
    async fn owned(&self, id: &str) -> Result<Arc<process::Process>, Error> {
        self.sessions
            .get(id)
            .await
            .ok_or_else(|| Error::forbidden("HARNESS_NOT_OWNED"))
    }
    async fn steer(&self, args: &Value) -> Result<Value, Error> {
        let process = self.owned(string(args, "sessionId")?).await?;
        let target = string(args, "turnId")?;
        if process.session.lock().unwrap()["activeTurnId"] != target {
            return Err(Error::busy());
        }
        let prompt = string(args, "prompt")?;
        let input = crate::harness::attachments::input(prompt, &args["attachments"])?;
        process
            .steer(
                &uuid()?,
                target,
                inputs(&input)?,
                args["requestId"].as_str(),
            )
            .await
    }
    async fn turn(&self, args: &Value) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        let process = self.owned(id).await?;
        if process.session.lock().unwrap()["status"] != "idle" {
            return Err(Error::busy());
        }
        let prompt = string(args, "prompt")?;
        if prompt.len() > 128 * 1024 {
            return Err(Error::invalid());
        }
        let input = crate::harness::attachments::input(prompt, &args["attachments"])?;
        let content = inputs(&input)?;
        let turn = uuid()?;
        if let Some(runtime) = &self.runtime {
            runtime.begin(id, &turn)?;
            runtime.bind_turn(id, &turn, &turn);
        }
        let result = async {
            self.select_model(&process, args).await?;
            process
                .turn(&turn, content, args["requestId"].as_str())
                .await
        }
        .await;
        if let (Err(error), Some(runtime)) = (&result, &self.runtime) {
            runtime.submission_failed(id, &turn, error);
        }
        result
    }
    async fn respond(&self, args: &Value) -> Result<Value, Error> {
        let request_id = args["nativeRequestId"]
            .as_str()
            .ok_or_else(Error::invalid)?;
        let request = self
            .events
            .lock()
            .unwrap()
            .requests
            .get(&json!(request_id).to_string())
            .cloned()
            .ok_or_else(Error::invalid)?;
        let process = self.owned(string(&request["params"], "threadId")?).await?;
        let response = args["response"].as_object().ok_or_else(Error::invalid)?;
        if request["method"] != "item/tool/requestUserInput"
            && !response
                .get("decision")
                .and_then(Value::as_str)
                .is_some_and(|v| ["accept", "decline", "cancel"].contains(&v))
        {
            return Err(Error::invalid());
        }
        process.respond(request_id, args["response"].clone()).await
    }
    async fn search(&self, args: &Value) -> Result<Value, Error> {
        let query = string(args, "query")?;
        if query.trim().is_empty() || query.len() > 1024 || query.contains('\0') {
            return Err(Error::invalid());
        }
        let mode = string(args, "mode")?;
        if !["title", "content", "prompt"].contains(&mode) {
            return Err(Error::invalid());
        }
        if args["archived"] == true {
            return Ok(json!({"matches":[],"truncated":false,"nextCursor":null}));
        }
        let entries = self.catalog().await?.entries;
        let config = self.config.clone();
        let query = query.to_lowercase();
        let mode = mode.to_owned();
        let runtime = self.runtime.clone();
        static SEARCHES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
        let permit = SEARCHES
            .try_acquire()
            .map_err(|_| Error::too_many("SEARCH_BUSY"))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            search::collect(&config, runtime.as_deref(), entries, &query, &mode)
        })
        .await
        .map_err(|_| Error::internal())?
    }
    fn check_read(&self) -> Result<(), Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::unavailable());
        }
        if let Some(runtime) = &self.runtime {
            runtime.state()?;
        }
        Ok(())
    }
    async fn read_command(&self, name: &str, args: &Value) -> Result<Value, Error> {
        match name {
            "harness_query" => self.query(args).await,
            "harness_session_metrics" => self.metrics(args).await,
            "harness_models" => self.models(args).await,
            "harness_commands" => self.commands(args).await,
            "harness_sessions" => self.list(args).await,
            "harness_session_read" => self.read_history(args).await,
            "harness_session_usage" => self.usage(args).await,
            "harness_session_info" => {
                let mut session = self.session(string(args, "sessionId")?).await?;
                if args["digestProjection"] != true {super::usage::attach(&mut session, self.usage(args).await);}
                Ok(json!({"session":session}))
            }
            "harness_session_search" => self.search(args).await,
            _ => Err(Error::unsupported()),
        }
    }
    async fn execute_command(&self, name: &str, args: &Value) -> Result<Value, Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::unavailable());
        }
        match name {
            "harness_create" => self.create(args).await,
            "harness_resume" => {
                self.attach(self.session(string(args, "sessionId")?).await?, true)
                    .await
            }
            "harness_steer" => self.steer(args).await,
            "harness_turn" => self.turn(args).await,
            "harness_command" => self.command(args).await,
            "harness_respond" => self.respond(args).await,
            "harness_interrupt" => self.interrupt(args).await,
            _ => Err(Error::unsupported()),
        }
    }
    async fn interrupt(&self, args: &Value) -> Result<Value, Error> {
        let process = self.owned(string(args, "sessionId")?).await?;
        if process.session.lock().unwrap()["activeTurnId"] != args["turnId"] {
            return Err(Error::invalid());
        }
        process.control(json!({"subtype":"interrupt"})).await
    }
}
impl HarnessDriver for Claude {
    fn subscribe(&self, args: Value) -> crate::plugins::EventFuture<'_> {
        Box::pin(async move {
            self.check_read()?;
            Ok(crate::harness::events::stream(&self.events, args))
        })
    }
    fn descriptor(&self) -> Value {
        json!({"id":self.config.id,"kind":"claude","workspaces":self.config.workspaces.iter().map(|w|json!({"id":w.id})).collect::<Vec<_>>(),"projectRuntime":self.config.projects,
        "capabilities":{"metrics":true,"models":true,"slashCommands":true,"commandQueries":true,"usage":true,"history":true,"create":true,"resume":true,"interrupt":true,"steer":true,"interactions":true,"search":true,"searchPrompts":true,"searchKeywords":true,"fork":false,"rename":false,"archive":false,"unarchive":false,"attachments":["text","image"]}})
    }
    fn read<'a>(&'a self, name: &'a str, args: Value) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            self.check_read()?;
            let result = self.read_command(name, &args).await?;
            self.check_read()?;
            Ok(result)
        })
    }
    fn execute<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(self.execute_command(name, args))
    }
    fn stop(&self) {
        self.closed.store(true, Ordering::Release);
        for process in self.sessions.live() {
            process.close();
        }
    }
    fn idle_since(&self) -> Option<std::time::Instant> {
        if self
            .sessions
            .live()
            .iter()
            .any(|process| process.session.lock().unwrap()["activeTurnId"].is_string())
        {
            return None;
        }
        self.events.lock().unwrap().idle_since()
    }
    fn drain(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.sessions.drain())
    }
}
fn uuid() -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| Error::internal())?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}
fn inputs(input: &Value) -> Result<Value, Error> {
    input
        .as_array()
        .ok_or_else(Error::invalid)?
        .iter()
        .map(|item| match item["type"].as_str() {
            Some("text") => Ok(json!({"type":"text","text":item["text"]})),
            Some("image") => {
                let url = string(item, "url")?;
                let (prefix, data) = url.split_once(",").ok_or_else(Error::invalid)?;
                let mime = prefix
                    .strip_prefix("data:")
                    .and_then(|s| s.strip_suffix(";base64"))
                    .ok_or_else(Error::invalid)?;
                Ok(json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}))
            }
            _ => Err(Error::invalid()),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|items| json!(items))
}
fn list_page(mut sessions: Vec<Value>, args: &Value) -> Result<Value, Error> {
    sessions.sort_by(|a, b| {
        b["updatedAt"]
            .as_i64()
            .cmp(&a["updatedAt"].as_i64())
            .then(a["id"].as_str().cmp(&b["id"].as_str()))
    });
    let offset = args["cursor"]
        .as_str()
        .map(|v| v.parse::<usize>().map_err(|_| Error::invalid()))
        .transpose()?
        .unwrap_or(0);
    let limit = args["limit"].as_u64().unwrap_or(100).clamp(1, 100) as usize;
    if offset > sessions.len() {
        return Err(Error::invalid());
    }
    let end = (offset + limit).min(sessions.len());
    Ok(
        json!({"sessions":sessions[offset..end],"nextCursor":if end<sessions.len(){json!(end.to_string())}else{Value::Null}}),
    )
}
