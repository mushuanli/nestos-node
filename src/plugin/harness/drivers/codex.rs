//! Codex-specific session semantics and native app-server protocol.
use super::{
    bridge::Bridge,
    config::{ProfileConfig, WorkspaceConfig},
    driver::HarnessDriver,
    service::string,
};
use crate::core::error::Error;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::Mutex;

pub(super) const SOURCE_KINDS: &[&str] = &[
    "cli",
    "vscode",
    "exec",
    "appServer",
    "subAgent",
    "subAgentReview",
    "subAgentCompact",
    "subAgentThreadSpawn",
    "subAgentOther",
    "unknown",
];

pub struct Codex {
    selections: Mutex<std::collections::BTreeMap<String, Value>>,
    config: ProfileConfig,
    runtime: Option<Arc<crate::projects::runtime::ProjectRuntime>>,
    bridge: Mutex<Option<Arc<Bridge>>>,
    pub(super) owned: Mutex<HashSet<String>>,
    closed: AtomicBool,
    created: std::time::Instant,
    usage: Arc<super::usage::Reader>,
}
impl Codex {
    pub fn new(config: ProfileConfig) -> Self {
        Self {
            selections: Mutex::new(std::collections::BTreeMap::new()),
            config,
            runtime: None,
            bridge: Mutex::new(None),
            owned: Mutex::new(HashSet::new()),
            closed: AtomicBool::new(false),
            created: std::time::Instant::now(),
            usage: Arc::default(),
        }
    }
    pub fn project(
        mut config: ProfileConfig,
        runtime: Arc<crate::projects::runtime::ProjectRuntime>,
    ) -> Self {
        config.workspaces = vec![WorkspaceConfig {
            id: runtime.project.id.clone(),
            path: runtime.cwd().into(),
        }];
        let mut codex = Self::new(config);
        codex.runtime = Some(runtime);
        codex
    }
    async fn bridge(&self) -> Result<Arc<Bridge>, Error> {
        let mut slot = self.bridge.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::unavailable());
        }
        if let Some(bridge) = slot.as_ref() {
            return Ok(bridge.clone());
        }
        let bridge = match &self.runtime {
            Some(runtime) => Bridge::start_scoped(&self.config, Some(runtime.clone())).await?,
            None => Bridge::start(&self.config).await?,
        };
        *slot = Some(bridge.clone());
        Ok(bridge)
    }
    async fn read(&self, name: &str, args: Value) -> Result<Value, Error> {
        let bridge = self.bridge().await?;
        match name {
            "harness_query" => self.query(&bridge, &args).await,
            "harness_commands" => self.commands(&bridge, &args).await,
            "harness_models" => self.models(&bridge, &args).await,
            "harness_session_search" => super::search::search(self, &bridge, &args).await,
            "harness_sessions" => self.sessions(&bridge, &args).await,
            "harness_session_read" => self.history(&bridge, &args).await,
            "harness_session_usage" => self.usage(&bridge, &args).await,
            "harness_session_info" => {
                let thread = self.summary(&bridge, string(&args, "sessionId")?).await?;
                let mut session = self.session(&thread);
                session["writerLock"] = super::writer_lock::inspect(&self.config.home, string(&args, "sessionId")?);
                super::usage::attach(&mut session, self.thread_usage(&thread).await);
                Ok(json!({"session":session}))
            }
            _ => Err(Error::unsupported()),
        }
    }
    pub(super) async fn sessions(&self, bridge: &Bridge, args: &Value) -> Result<Value, Error> {
        let limit = args["limit"].as_u64().unwrap_or(25).clamp(1, 100);
        let result = bridge
            .call(
                "thread/list",
                json!({"cursor":args["cursor"],"limit":limit,
            "archived":args["archived"].as_bool().unwrap_or(false),"sourceKinds":SOURCE_KINDS,
            "sortKey":"updated_at"}),
            )
            .await?;
        let sessions = result["data"]
            .as_array()
            .ok_or_else(Error::internal)?
            .iter()
            .filter(|thread| {
                thread["cwd"]
                    .as_str()
                    .is_some_and(|cwd| self.authorized(cwd))
            })
            .map(|thread| self.session(&self.runtime_thread(bridge, thread)))
            .collect::<Vec<_>>();
        Ok(json!({"sessions":sessions,"nextCursor":result["nextCursor"]}))
    }
    fn session(&self, thread: &Value) -> Value {
        let mut metadata = thread.clone();
        if let Some(fields) = metadata.as_object_mut() {
            fields.remove("turns");
        }
        json!({"id":thread["id"],"title":super::presentation::session_title(thread),
            "loaded":thread["status"]["type"] != "notLoaded",
            "cwd":thread["cwd"],"status":thread["status"]["type"],"statusDetails":thread["status"],"createdAt":super::presentation::time_ms(&thread["createdAt"]),
            "updatedAt":super::presentation::time_ms(&thread["updatedAt"]),
            "branchName":if thread["forkedFromId"].is_string() {thread["name"].clone()} else {Value::Null},
            "parentSessionId":thread["forkedFromId"],"forkable":thread["cwd"].as_str().is_some_and(|cwd|self.authorized(cwd)),
            "owned":self.owned.try_lock().is_ok_and(|owned|thread["id"].as_str().is_some_and(|id|owned.contains(id))),
            "activeTurnId":thread.get("activeTurnId").cloned().unwrap_or_else(||thread["turns"].as_array().and_then(|turns|turns.iter().find(|turn|turn["status"]=="inProgress")).map(|turn|turn["id"].clone()).unwrap_or(Value::Null)),
            "lastTurnId":thread["lastTurnId"],
            "lastTurnResult":thread.get("lastTurnResult").cloned().unwrap_or_else(||thread["turns"].as_array().and_then(|turns| turns.last()).and_then(|turn| match turn["status"].as_str() {Some("completed")=>Some("completed"),Some("failed")=>Some("failed"),Some("interrupted")=>Some("cancelled"),_=>None}).map(Value::from).unwrap_or(Value::Null)),
            "archived":thread["archived"].as_bool().unwrap_or_else(||thread["path"].as_str().is_some_and(|path| path.contains("/archived_sessions/"))),
            "resumable":thread["cwd"].as_str().is_some_and(|cwd| self.authorized(cwd)),"native":metadata})
    }
    pub(super) fn authorized(&self, cwd: &str) -> bool {
        if let Some(runtime) = &self.runtime {
            return runtime.resolve_cwd(cwd).is_ok();
        }
        std::path::Path::new(cwd)
            .canonicalize()
            .ok()
            .is_some_and(|path| {
                self.config
                    .workspaces
                    .iter()
                    .any(|w| path.starts_with(&w.path))
            })
    }
    fn workspace(&self, id: &str) -> Result<&WorkspaceConfig, Error> {
        self.config
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .ok_or_else(|| Error::forbidden("EACCES"))
    }
}
impl Codex {
    async fn execute(&self, bridge: &Bridge, name: &str, args: &Value) -> Result<Value, Error> {
        match name {
            "harness_create" => self.create(bridge, args).await,
            "harness_resume" => self.resume(bridge, args).await,
            "harness_fork" => self.fork(bridge, args).await,
            "harness_rename" | "harness_archive" | "harness_unarchive" | "harness_delete" => {
                self.manage(bridge, name, args).await
            }
            "harness_turn" => self.turn(bridge, args).await,
            "harness_command" => self.command(bridge, args).await,
            "harness_interrupt" => {
                self.require_owned(string(args, "sessionId")?).await?;
                bridge.persistent_call("turn/interrupt",json!({"threadId":string(args,"sessionId")?,"turnId":string(args,"turnId")?})).await
            }
            "harness_respond" => self.respond(bridge, args).await,
            _ => Err(Error::unsupported()),
        }
    }
    async fn create(&self, bridge: &Bridge, args: &Value) -> Result<Value, Error> {
        let workspace = self.workspace(string(args, "workspaceId")?)?;
        let result = bridge.persistent_call("thread/start",json!({"cwd":workspace.path,"approvalPolicy":"on-request","sandbox":if self.runtime.is_some(){"danger-full-access"}else{"workspace-write"},"historyMode":"legacy"})).await?;
        self.owned.lock().await.insert(
            string(&result["thread"], "id")
                .map_err(|_| Error::internal())?
                .into(),
        );
        self.remember_model(&result).await;
        Ok(json!({"session":self.session(&result["thread"])}))
    }
    async fn resume(&self, bridge: &Bridge, args: &Value) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        let stored = bridge
            .call("thread/read", json!({"threadId":id,"includeTurns":false}))
            .await?;
        if self.owned.lock().await.contains(id) {
            return Ok(json!({"session":self.session(&stored["thread"])}));
        }
        let cwd = string(&stored["thread"], "cwd")?;
        if !self.authorized(cwd) {
            return Err(Error::forbidden("EACCES"));
        }
        if stored["thread"]["status"]["type"] == "active" {
            return Err(Error::busy());
        }
        let translated = self
            .runtime
            .as_ref()
            .map(|runtime| runtime.resolve_cwd(cwd))
            .transpose()?;
        let cwd = translated.as_deref().unwrap_or(cwd);
        let result = bridge.persistent_call("thread/resume",json!({"threadId":id,"cwd":cwd,"excludeTurns":true,"approvalPolicy":"on-request","sandbox":if self.runtime.is_some(){"danger-full-access"}else{"workspace-write"}})).await?;
        self.owned.lock().await.insert(id.into());
        self.remember_model(&result).await;
        Ok(json!({"session":self.session(&result["thread"])}))
    }
    pub(super) async fn require_owned(&self, id: &str) -> Result<(), Error> {
        if !self.owned.lock().await.contains(id) {
            return Err(Error::forbidden("HARNESS_SESSION_NOT_OWNED"));
        }
        Ok(())
    }
    async fn turn(&self, bridge: &Bridge, args: &Value) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        self.require_owned(id).await?;
        let prompt = string(args, "prompt")?;
        if prompt.is_empty() || prompt.len() > 128 * 1024 {
            return Err(Error::invalid());
        }
        let input = super::attachments::input(prompt, &args["attachments"])?;
        self.start_input(bridge, args, json!(input)).await
    }
    async fn start_input(
        &self,
        bridge: &Bridge,
        args: &Value,
        input: Value,
    ) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        self.require_owned(id).await?;
        let params = self.turn_params(bridge, args, input).await?;
        let operation = string(args, "requestId")?;
        if let Some(runtime) = &self.runtime {
            runtime.begin(id, operation)?;
        }
        let result = bridge.persistent_call("turn/start", params.clone()).await;
        if let (Err(error), Some(runtime)) = (&result, &self.runtime) {
            runtime.submission_failed(id, operation, error);
        }
        let result = result?;
        let turn = string(&result["turn"], "id").map_err(|_| Error::internal())?;
        self.remember_turn_model(id, &params).await;
        if let Some(runtime) = &self.runtime {
            runtime.bind_turn(id, operation, turn);
        }
        Ok(json!({"turnId":turn}))
    }
    async fn respond(&self, bridge: &Bridge, args: &Value) -> Result<Value, Error> {
        let id = args.get("nativeRequestId").ok_or_else(Error::invalid)?;
        let request = bridge
            .events
            .lock()
            .unwrap()
            .requests
            .get(&id.to_string())
            .cloned()
            .ok_or_else(|| Error::not_found("ENOENT"))?;
        self.require_owned(string(&request["params"], "threadId")?)
            .await?;
        let result = args.get("response").ok_or_else(Error::invalid)?.clone();
        validate_response(&request, result.clone())?;
        bridge.respond(id.clone(), result).await?;
        bridge
            .events
            .lock()
            .unwrap()
            .requests
            .remove(&id.to_string());
        Ok(json!({}))
    }
}

impl Codex {
    async fn manage(&self, bridge: &Bridge, name: &str, args: &Value) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        let thread = self.summary(bridge, id).await?;
        if name == "harness_delete" {
            return self.delete_native(bridge, thread).await;
        }
        let thread = match name {
            "harness_rename" => self.rename_native(bridge, thread, args).await?,
            "harness_archive" => self.archive_native(bridge, thread).await?,
            "harness_unarchive" => self.unarchive_native(bridge, id).await?,
            _ => return Err(Error::unsupported()),
        };
        Ok(json!({"session":self.session(&thread)}))
    }
    async fn rename_native(
        &self,
        bridge: &Bridge,
        mut thread: Value,
        args: &Value,
    ) -> Result<Value, Error> {
        let title = string(args, "name")?.trim();
        if title.is_empty() || title.len() > 512 || title.chars().any(char::is_control) {
            return Err(Error::invalid());
        }
        bridge
            .persistent_call(
                "thread/name/set",
                json!({"threadId":thread["id"],"name":title}),
            )
            .await?;
        thread["name"] = json!(title);
        Ok(thread)
    }
    async fn archive_native(&self, bridge: &Bridge, mut thread: Value) -> Result<Value, Error> {
        let id = string(&thread, "id")?;
        self.archivable(&thread).await?;
        for archived in [false, true] {
            self.management_descendants(bridge, id, archived, false)
                .await?;
        }
        bridge
            .persistent_call("thread/archive", json!({"threadId":id}))
            .await?;
        self.owned.lock().await.remove(id);
        thread["archived"] = json!(true);
        Ok(thread)
    }
    async fn unarchive_native(&self, bridge: &Bridge, id: &str) -> Result<Value, Error> {
        let result = bridge
            .persistent_call("thread/unarchive", json!({"threadId":id}))
            .await?;
        let mut thread = result["thread"].clone();
        if thread["id"] != id
            || !thread["cwd"]
                .as_str()
                .is_some_and(|cwd| self.authorized(cwd))
        {
            return Err(Error::unavailable());
        }
        thread["archived"] = json!(false);
        Ok(thread)
    }
    async fn summary(&self, bridge: &Bridge, id: &str) -> Result<Value, Error> {
        let summary = bridge
            .call("thread/read", json!({"threadId":id,"includeTurns":false}))
            .await?;
        let thread = summary["thread"].clone();
        if !thread["cwd"]
            .as_str()
            .is_some_and(|cwd| self.authorized(cwd))
        {
            return Err(Error::forbidden("EACCES"));
        }
        Ok(self.runtime_thread(bridge, &thread))
    }
    fn runtime_thread(&self, bridge: &Bridge, thread: &Value) -> Value {
        let mut thread = thread.clone();
        let metadata = bridge
            .events
            .lock()
            .unwrap()
            .runtime_metadata(thread["id"].as_str().unwrap_or(""));
        if thread["status"]["type"] == "active" && metadata["activeTurnId"].is_string() {
            thread["activeTurnId"] = metadata["activeTurnId"].clone();
        }
        for key in ["lastTurnId", "lastTurnResult"] {
            if !metadata[key].is_null() {
                thread[key] = metadata[key].clone();
            }
        }
        thread
    }
    pub(super) async fn history(&self, bridge: &Bridge, args: &Value) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        let mut thread = self.summary(bridge, id).await?;
        let mut page = self.page(bridge, &thread, args).await?;
        if args["toolDetail"] == "summary" {
            if let Some(turns) = page["turns"].as_array_mut() {
                for turn in turns {
                    if let Some(items) = turn["items"].as_array_mut() {
                        *items = items.iter().filter_map(super::presentation::item).collect();
                    }
                }
            }
        }
        thread["turns"] = page["turns"].clone();
        let mut session = self.session(&thread);
        session["writerLock"] = super::writer_lock::inspect(&self.config.home, id);
        if !args["cursor"].is_string() {
            super::usage::attach(&mut session, self.thread_usage(&thread).await);
        }
        let mut result =
            json!({"session":session,"turns":page["turns"],"nextCursor":page["nextCursor"]});
        if page["eventCursor"].is_object() {
            result["eventCursor"] = page["eventCursor"].clone();
        }
        Ok(result)
    }
    async fn page(&self, bridge: &Bridge, thread: &Value, args: &Value) -> Result<Value, Error> {
        // Rollout files may not contain the answer while the native turn is streaming.
        if !args["cursor"].is_string() && thread["status"]["type"] == "active" {
            return self.native_page(bridge, thread, args).await;
        }
        if args["toolDetail"] == "summary" && thread["path"].is_string() {
            match self.rollout(thread, args).await {
                Ok(page) => return Ok(page),
                Err(error) if error.code == "ENOENT" => (),
                Err(error) => return Err(error),
            }
        }
        if thread["historyMode"] == "paginated" || args["cursor"].is_string() {
            return self.rollout(thread, args).await;
        }
        self.native_page(bridge, thread, args).await
    }
    async fn usage(&self, bridge: &Bridge, args: &Value) -> Result<Value, Error> {
        let id = string(args, "sessionId")?.to_owned();
        let thread = self.summary(bridge, &id).await?;
        self.thread_usage(&thread).await
    }
    async fn thread_usage(&self, thread: &Value) -> Result<Value, Error> {
        let id = string(thread, "id")?.to_owned();
        let home = self.config.home.clone();
        let path = self.history_path(thread)?;
        let reader = self.usage.clone();
        let permit = super::usage::permit()?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let file = super::history::open_log(&home, &path)?;
            reader.read(file, &id, super::usage::Kind::Codex, |_| Ok(()))
        })
        .await
        .map_err(|_| Error::internal())?
    }
    fn history_path(&self, thread: &Value) -> Result<String, Error> {
        let path = string(thread, "path")?;
        Ok(if self.runtime.is_some() {
            self.config
                .home
                .join(path.strip_prefix("/harness/").unwrap_or(path))
                .to_string_lossy()
                .into_owned()
        } else {
            path.to_owned()
        })
    }
    async fn native_page(
        &self,
        bridge: &Bridge,
        thread: &Value,
        args: &Value,
    ) -> Result<Value, Error> {
        Ok(
            match bridge
                .call(
                    "thread/read",
                    json!({"threadId":string(args,"sessionId")?,"includeTurns":true}),
                )
                .await
            {
                Ok(full) => {
                    json!({"turns":full["thread"]["turns"],"nextCursor":null,"eventCursor":full["_nestosNodeEventCursor"]})
                }
                Err(error) if error.code == "HARNESS_HISTORY_PENDING" => {
                    json!({"turns":[],"nextCursor":null})
                }
                Err(error) if error.code == "ECAPABILITY" => self.rollout(thread, args).await?,
                Err(error) => return Err(error),
            },
        )
    }
    async fn rollout(&self, thread: &Value, args: &Value) -> Result<Value, Error> {
        let translated = self.history_path(thread)?;
        let cursor = args
            .get("cursor")
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_str()
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(Error::invalid)
            })
            .transpose()?;
        super::history::read(
            &self.config.home,
            &translated,
            cursor,
            args["toolDetail"] == "summary",
        )
        .await
    }
    fn fork_cwd(&self, thread: &Value) -> Result<String, Error> {
        let cwd = string(thread, "cwd")?;
        if !self.authorized(cwd) {
            return Err(Error::forbidden("EACCES"));
        }
        if thread["status"]["type"] == "active" {
            return Err(Error::busy());
        }
        Ok(self
            .runtime
            .as_ref()
            .map(|runtime| runtime.resolve_cwd(cwd))
            .transpose()?
            .unwrap_or_else(|| cwd.to_owned()))
    }
    async fn fork(&self, bridge: &Bridge, args: &Value) -> Result<Value, Error> {
        let thread = self.summary(bridge, string(args, "sessionId")?).await?;
        let cwd = self.fork_cwd(&thread)?;
        self.require_checkpoint(bridge, args).await?;
        let mut params = json!({"threadId":string(args,"sessionId")?,"cwd":cwd,"excludeTurns":true,
            "approvalPolicy":"on-request","sandbox":if self.runtime.is_some(){"danger-full-access"}else{"workspace-write"}});
        if let Some(checkpoint) = args.get("lastTurnId") {
            params["lastTurnId"] = checkpoint.clone();
        }
        let result = bridge.persistent_call("thread/fork", params).await?;
        let mut created = result["thread"].clone();
        let id = string(&created, "id")
            .map_err(|_| Error::internal())?
            .to_owned();
        self.owned.lock().await.insert(id.clone());
        self.name_branch(bridge, args, &mut created, &id).await;
        let mut session = self.session(&created);
        let mut result = json!({});
        if let Some(checkpoint) = self.checkpoint_result(bridge, args, &mut session).await {
            result["checkpoint"] = checkpoint;
        }
        result["session"] = session;
        Ok(result)
    }
    async fn name_branch(&self, bridge: &Bridge, args: &Value, created: &mut Value, id: &str) {
        // A naming failure cannot turn an already committed fork into a replayable failure.
        if let Some(name) = args["name"]
            .as_str()
            .filter(|name| !name.trim().is_empty() && name.chars().count() <= 256)
        {
            if bridge
                .call("thread/name/set", json!({"threadId":id,"name":name}))
                .await
                .is_ok()
            {
                created["name"] = json!(name);
            }
        }
    }
}

fn validate_response(request: &Value, result: Value) -> Result<(), Error> {
    match request["method"].as_str().unwrap_or("") {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            if !matches!(
                result["decision"].as_str(),
                Some("accept" | "acceptForSession" | "decline" | "cancel")
            ) {
                return Err(Error::invalid());
            }
        }
        "item/tool/requestUserInput" => {
            if !result["answers"].is_object() {
                return Err(Error::invalid());
            }
        }
        _ => return Err(Error::unsupported()),
    }
    Ok(())
}

impl HarnessDriver for Codex {
    fn subscribe(&self, args: Value) -> crate::plugins::EventFuture<'_> {
        Box::pin(async move { Ok(super::events::stream(&self.bridge().await?.events, args)) })
    }
    fn descriptor(&self) -> Value {
        json!({"id":self.config.id,"kind":self.config.kind,
            "workspaces":self.config.workspaces.iter().map(|w| json!({"id":w.id})).collect::<Vec<_>>(),
            "projectRuntime":self.config.projects,
            "capabilities":{"models":true,"slashCommands":true,"commandQueries":true,"usage":true,"history":true,"create":true,"resume":true,"interrupt":true,"interactions":true,"fork":true,"checkpointFork":true,"search":true,"searchPrompts":true,"searchKeywords":true,"rename":true,"archive":true,"unarchive":true,"delete":true,"attachments":["text","image"]}})
    }
    fn read<'a>(&'a self, name: &'a str, args: Value) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move { self.read(name, args).await })
    }
    fn execute<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            let bridge = self.bridge().await?;
            self.execute(&bridge, name, args).await
        })
    }
    fn stop(&self) {
        self.closed.store(true, Ordering::Release);
    }
    fn idle_since(&self) -> Option<std::time::Instant> {
        let bridge = self.bridge.try_lock().ok()?;
        match bridge.as_ref() {
            Some(bridge) => bridge.events.lock().unwrap().idle_since(),
            None => Some(self.created),
        }
    }
    fn drain(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Some(bridge) = self.bridge.lock().await.as_ref() {
                bridge.drain().await;
            }
        })
    }
}
#[path = "codex/checkpoints.rs"]
mod checkpoints;
#[path = "codex/commands.rs"]
mod commands;
#[path = "codex/models.rs"]
mod models;
