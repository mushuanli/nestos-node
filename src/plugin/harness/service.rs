use super::mutation_queue::{MutationGuard, MutationQueues};
use super::receipts::{ReceiptScope, Receipts, Reservation};
use super::{config::ProfileConfig, driver_slot::DriverSlot, HarnessPlugins};
use crate::app::State;
use crate::core::error::Error;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::Mutex;

pub struct Harnesses {
    profiles: BTreeMap<String, Arc<Profile>>,
    configs: Vec<ProfileConfig>,
    scoped: Mutex<BTreeMap<String, Arc<Profile>>>,
    epoch: String,
    closed: AtomicBool,
    plugins: HarnessPlugins,
    receipts: Arc<Receipts>,
}
pub(super) struct Profile {
    pub(super) driver: DriverSlot,
    receipts: ReceiptScope,
    mutations: MutationQueues,
}

impl Harnesses {
    pub fn maintain(state: std::sync::Weak<crate::app::State>) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut timer = tokio::time::interval(std::time::Duration::from_secs(60));
                timer.tick().await;
                loop {
                    timer.tick().await;
                    let Some(state) = state.upgrade() else {
                        break;
                    };
                    let Some(harness) = state.services.get::<Harnesses>() else {
                        break;
                    };
                    if harness.closed.load(Ordering::Acquire) {
                        break;
                    }
                    harness.reap_idle(std::time::Instant::now()).await;
                }
            });
        }
    }
    pub async fn reap_idle(&self, now: std::time::Instant) {
        let ttl = std::time::Duration::from_secs(300);
        let mut profiles: Vec<_> = self.profiles.values().cloned().collect();
        profiles.extend(self.scoped.lock().await.values().cloned());
        for profile in profiles {
            if let Some(driver) = profile.driver.idle(now, ttl) {
                driver.drain().await;
            }
        }
    }
    pub fn new(configs: Vec<ProfileConfig>, epoch: String) -> Result<Self, String> {
        Self::with_plugins(configs, epoch, HarnessPlugins::default())
    }
    pub fn with_plugins(
        configs: Vec<ProfileConfig>,
        epoch: String,
        plugins: HarnessPlugins,
    ) -> Result<Self, String> {
        let receipts = Receipts::new()?;
        let profiles = configs
            .clone()
            .into_iter()
            .map(|config| {
                let scope = receipts.scoped(json!(["installation", config.id]).to_string());
                Ok((
                    config.id.clone(),
                    Arc::new(Profile {
                        driver: DriverSlot::new(plugins.clone(), config, None)?,
                        receipts: scope,
                        mutations: MutationQueues::default(),
                    }),
                ))
            })
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        Ok(Self {
            profiles,
            configs,
            scoped: Mutex::new(BTreeMap::new()),
            epoch,
            closed: AtomicBool::new(false),
            plugins,
            receipts,
        })
    }
    pub fn enabled(&self) -> bool {
        !self.profiles.is_empty()
    }
    pub fn private_homes(&self) -> impl Iterator<Item = &std::path::Path> {
        self.configs.iter().map(|profile| profile.home.as_path())
    }
    pub fn profiles(&self) -> Value {
        let profiles = self
            .configs
            .iter()
            .map(|config| {
                let mut descriptor = self.profiles[&config.id].driver.descriptor();
                descriptor["capabilities"]["commandOutputs"] = json!(true);
                if !config.aliases.is_empty() {
                    descriptor["aliases"] = json!(config.aliases);
                }
                descriptor
            })
            .collect::<Vec<_>>();
        json!({"epoch":self.epoch,"profiles":profiles})
    }
    pub(crate) fn workspaces(&self, client: &crate::auth::Client) -> Vec<Value> {
        self.configs
            .iter()
            .filter(|profile| client.may_harness(&profile.id))
            .flat_map(|profile| {
                profile.workspaces.iter().map(|workspace| {
                    json!({"profileId":profile.id,"workspaceId":workspace.id,
                        "capabilities":{"sessionCreate":true}})
                })
            })
            .collect()
    }
    fn canonical_args(&self, mut args: Value) -> Result<Value, Error> {
        let id = string(&args, "profileId")?;
        let profile = self
            .configs
            .iter()
            .find(|p| p.id == id || p.aliases.iter().any(|alias| alias == id))
            .ok_or_else(|| Error::not_found("ENOENT"))?;
        args["profileId"] = json!(profile.id);
        Ok(args)
    }
    pub async fn authorized_call(
        &self,
        state: &State,
        identity: usize,
        name: &str,
        args: Value,
    ) -> Result<Value, Error> {
        let client = state.auth.try_client(identity)?;
        if name == "harness_profiles" {
            let mut result = self.profiles();
            result["profiles"]
                .as_array_mut()
                .unwrap()
                .retain(|p| p["id"].as_str().is_some_and(|id| client.may_harness(id)));
            return Ok(result);
        }
        if !client.legacy() {
            return Err(Error::forbidden("PROJECT_CONTEXT_REQUIRED"));
        }
        let args = self.canonical_args(args)?;
        if !client.may_harness(string(&args, "profileId")?) {
            return Err(Error::forbidden("EACCES"));
        }
        self.call(name, args).await
    }
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::unavailable());
        }
        if name == "harness_profiles" {
            return Ok(self.profiles());
        }
        let args = self.canonical_args(args)?;
        let profile = self
            .profiles
            .get(string(&args, "profileId")?)
            .ok_or_else(|| Error::not_found("ENOENT"))?;
        self.profile_call(profile, name, args).await
    }
    async fn profile_call(
        &self,
        profile: &Arc<Profile>,
        name: &str,
        args: Value,
    ) -> Result<Value, Error> {
        let referenced = output_mode(name, &args)?;
        if name == "harness_operation" {
            self.check_epoch(&args)?;
            return profile
                .operation(string(&args, "requestId")?)
                .await
                .map(|value| self.project_receipt(name, value, referenced));
        }
        if name == "harness_command_output" {
            self.check_epoch(&args)?;
            return profile.receipts.output(args, self.epoch.clone()).await;
        }
        if is_mutation(name) {
            self.check_epoch(&args)?;
            return profile
                .mutate(name, args)
                .await
                .map(|value| self.project_receipt(name, value, referenced));
        }
        profile.read(name, args).await
    }
    fn project_receipt(&self, name: &str, receipt: Value, referenced: bool) -> Value {
        if matches!(name, "harness_operation" | "harness_command") {
            super::command_outputs::project(receipt, &self.epoch, referenced)
        } else {
            super::command_outputs::public_receipt(receipt)
        }
    }
    fn project_context(
        &self,
        state: &Arc<State>,
        identity: usize,
        args: &Value,
    ) -> Result<(crate::projects::model::Project, ProfileConfig, ReceiptScope), Error> {
        let client = state.auth.try_client(identity)?;
        if !client.may_harness(string(args, "profileId")?) {
            return Err(Error::forbidden("EACCES"));
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::unavailable());
        }
        let project = state
            .services
            .require::<crate::projects::ProjectService>()?
            .get(identity, string(args, "projectId")?)?;
        crate::projects::service::authorize(state, identity, &project)?;
        let project = crate::projects::service::effective(state, identity, &project);
        let config = self
            .configs
            .iter()
            .find(|p| p.id == string(args, "profileId").unwrap_or("") && p.projects)
            .ok_or_else(Error::unsupported)?
            .clone();
        let receipts = self
            .receipts
            .scoped(json!(["project", project.owner, project.id, args["profileId"]]).to_string());
        Ok((project, config, receipts))
    }
    fn project_access(
        &self,
        state: &State,
        identity: usize,
        project: &mut crate::projects::model::Project,
        args: &Value,
        mutation: bool,
    ) -> Result<(), Error> {
        if mutation
            && crate::projects::service::mounts(project)
                .iter()
                .any(|m| !state.auth.client(identity).may_execute(&m.alias))
        {
            return Err(Error::forbidden("EACCES"));
        }
        if args["revision"].as_u64() != Some(project.revision) {
            return Err(Error::conflict("PROJECT_REVISION_CHANGED"));
        }
        if args["readOnly"].as_bool() == Some(true) {
            project.access = "ro".into();
            for mount in &mut project.mounts {
                mount.access = "ro".into();
            }
        }
        Ok(())
    }
    async fn project_profile(
        &self,
        state: &Arc<State>,
        identity: usize,
        project: crate::projects::model::Project,
        config: ProfileConfig,
        receipts: ReceiptScope,
        read_only: bool,
    ) -> Result<Arc<Profile>, Error> {
        let key = format!(
            "{}:{}:{}:{}",
            project.id, project.revision, config.id, read_only
        );
        let mut scoped = self.scoped.lock().await;
        if !scoped.contains_key(&key) {
            if scoped.len() >= 128 {
                return Err(Error::too_many("PROJECT_HARNESS_LIMIT"));
            }
            let runtime = Arc::new(crate::projects::runtime::ProjectRuntime::new(
                state, identity, project,
            ));
            scoped.insert(
                key.clone(),
                Arc::new(Profile {
                    driver: DriverSlot::new(self.plugins.clone(), config, Some(runtime))
                        .map_err(|_| Error::unsupported())?,
                    receipts,
                    mutations: MutationQueues::default(),
                }),
            );
        }
        Ok(scoped[&key].clone())
    }
    pub async fn project_call(
        &self,
        state: &Arc<State>,
        identity: usize,
        name: &str,
        args: Value,
    ) -> Result<Value, Error> {
        let args = self.canonical_args(args)?;
        let (mut project, config, receipts) = self.project_context(state, identity, &args)?;
        if name == "harness_command_output" {
            self.project_access(state, identity, &mut project, &args, false)?;
        }
        if let Some(receipt) = self.retained(name, &args, &receipts).await? {
            return Ok(receipt);
        }
        self.project_access(state, identity, &mut project, &args, is_mutation(name))?;
        let profile = self
            .project_profile(
                state,
                identity,
                project,
                config,
                receipts,
                args["readOnly"].as_bool().unwrap_or(false),
            )
            .await?;
        self.profile_call(&profile, name, args).await
    }
    async fn retained(
        &self,
        name: &str,
        args: &Value,
        receipts: &ReceiptScope,
    ) -> Result<Option<Value>, Error> {
        let referenced = output_mode(name, args)?;
        if name == "harness_operation" {
            self.check_epoch(args)?;
            return self
                .scoped_operation(receipts, args, referenced)
                .await
                .map(Some);
        }
        if name == "harness_command_output" {
            self.check_epoch(args)?;
            return receipts
                .output(args.clone(), self.epoch.clone())
                .await
                .map(Some);
        }
        if is_mutation(name) {
            self.check_epoch(args)?;
            return receipts
                .existing(string(args, "requestId")?, fingerprint(name, args))
                .await
                .map(|value| value.map(|receipt| self.project_receipt(name, receipt, referenced)));
        }
        Ok(None)
    }
    async fn scoped_operation(
        &self,
        receipts: &ReceiptScope,
        args: &Value,
        referenced: bool,
    ) -> Result<Value, Error> {
        let drivers = self
            .scoped
            .lock()
            .await
            .values()
            .filter(|profile| profile.receipts.same_scope(receipts))
            .filter_map(|profile| profile.driver.peek())
            .collect();
        reconcile_receipt(receipts, string(args, "requestId")?, drivers)
            .await
            .map(|receipt| self.project_receipt("harness_operation", receipt, referenced))
    }
    pub async fn subscribe(
        &self,
        state: &Arc<State>,
        identity: usize,
        args: Value,
    ) -> Result<crate::plugin::common::EventStream, Error> {
        let args = self.canonical_args(args)?;
        let profile = if args.get("projectId").is_some() {
            let (mut project, config, receipts) = self.project_context(state, identity, &args)?;
            self.project_access(state, identity, &mut project, &args, false)?;
            self.project_profile(
                state,
                identity,
                project,
                config,
                receipts,
                args["readOnly"].as_bool().unwrap_or(false),
            )
            .await?
        } else {
            let client = state.auth.try_client(identity)?;
            if !client.legacy() || !client.may_harness(string(&args, "profileId")?) {
                return Err(Error::forbidden("EACCES"));
            }
            self.profiles
                .get(string(&args, "profileId")?)
                .ok_or_else(Error::unsupported)?
                .clone()
        };
        let driver = profile.driver.get()?;
        let source = driver.subscribe(args.clone()).await?;
        drop(driver);
        let generation = profile.driver.changes();
        Ok(super::subscription::stream(
            profile, generation, args, source,
        ))
    }
    pub async fn close_project(&self, id: &str) {
        let profiles = {
            let scoped = self.scoped.lock().await;
            let profiles = scoped
                .iter()
                .filter(|(key, _)| key.starts_with(&format!("{id}:")))
                .map(|(key, profile)| (key.clone(), profile.clone()))
                .collect::<Vec<_>>();
            for (_, profile) in &profiles {
                profile.driver.stop();
            }
            profiles
        };
        // Keep fenced entries visible until cleanup ends, without locking other projects.
        futures_util::future::join_all(profiles.iter().map(|(_, p)| p.driver.drain())).await;
        let mut scoped = self.scoped.lock().await;
        for (key, profile) in profiles {
            if scoped
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &profile))
            {
                scoped.remove(&key);
            }
        }
    }
    fn check_epoch(&self, args: &Value) -> Result<(), Error> {
        if string(args, "epoch")? != self.epoch {
            return Err(Error::conflict("HARNESS_EPOCH_CHANGED"));
        }
        Ok(())
    }
    pub async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let mut scoped = self.scoped.lock().await;
        for profile in scoped.values() {
            profile.driver.stop();
        }
        for profile in scoped.values() {
            profile.driver.drain().await;
        }
        scoped.clear();
        // Fence every driver before waiting so queued mutations cannot start another child.
        for profile in self.profiles.values() {
            profile.driver.stop();
        }
        for profile in self.profiles.values() {
            profile.driver.drain().await;
        }
    }
}

impl Profile {
    async fn read(&self, name: &str, args: Value) -> Result<Value, Error> {
        let started = std::time::Instant::now();
        let result = match self.driver.get() {
            Ok(driver) => {
                super::diagnostics::scope(
                    super::diagnostics::operation_context(name, &args),
                    driver.read(name, args.clone()),
                )
                .await
            }
            Err(error) => Err(error),
        };
        if name == "harness_query" {
            let operation = args["requestId"].as_str().filter(|id| {
                id.len() <= 128
                    && id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            });
            crate::core::events::emit(
                crate::core::events::Level::Info,
                "harness.query.result",
                json!({"operation":"harness.query","operationId":operation,"stage":"native",
                    "outcome":if result.is_ok() {"succeeded"} else {"failed"},
                    "code":result.as_ref().err().map(|error|error.code),"durationMs":started.elapsed().as_millis()}),
            );
        }
        result
    }
    async fn mutate(self: &Arc<Self>, name: &str, args: Value) -> Result<Value, Error> {
        let id = string(&args, "requestId")?.to_owned();
        if !crate::core::ids::is_identifier_within(&id, 128) {
            return Err(Error::invalid());
        }
        let permit = match self.receipts.reserve(&id, fingerprint(name, &args)).await? {
            Reservation::Existing(receipt) => return Ok(receipt),
            Reservation::New(permit) => permit,
        };
        // Execution outlives an HTTP disconnect; queries can confirm a later result.
        let context = super::diagnostics::operation_context(name, &args);
        let work = tokio::spawn(super::diagnostics::scope(
            context,
            self.clone()
                .execute(name.to_owned(), args, id.clone(), permit),
        ));
        match tokio::time::timeout(std::time::Duration::from_secs(25), work).await {
            Ok(Ok(value)) => Ok(value),
            _ => Ok(json!({"outcome":"unknown","requestId":id})),
        }
    }
    async fn operation(&self, id: &str) -> Result<Value, Error> {
        reconcile_receipt(&self.receipts, id, self.driver.peek().into_iter().collect()).await
    }
    async fn execute(
        self: Arc<Self>,
        name: String,
        args: Value,
        id: String,
        _permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Value {
        let started = std::time::Instant::now();
        let context = super::diagnostics::operation_context(&name, &args);
        super::diagnostics::operation_started(&context);
        let mut dispatched = false;
        // Keep serialization through receipt persistence and the driver's settlement callback.
        let (result, _serial) = self.execute_native(&name, &args, &mut dispatched).await;
        if !dispatched {
            report_preparation_failure(&id, &name, &result, started);
        }
        if name == "harness_fork" && args.get("lastTurnId").is_some() {
            checkpoint_diagnostic(&id, &args, &result, started.elapsed());
        }
        report_command_result(&name, &id, &args, &result, dispatched, started);
        let receipt = self.commit(&id, result, dispatched).await;
        super::diagnostics::operation_result(&context, &receipt, started.elapsed());
        receipt
    }
    async fn commit(&self, id: &str, result: Result<Value, Error>, dispatched: bool) -> Value {
        let receipt = receipt(id, result, dispatched);
        let receipt = self.receipts.finish(id, receipt).await.unwrap_or_else(
            |_| json!({"outcome":"unknown","requestId":id,"code":"HARNESS_RECEIPT_STORAGE"}),
        );
        if let Some(driver) = self.driver.peek() {
            driver.operation_settled(id, &receipt);
        }
        receipt
    }
    async fn execute_native(
        &self,
        name: &str,
        args: &Value,
        dispatched: &mut bool,
    ) -> (Result<Value, Error>, Option<MutationGuard>) {
        let args = execution_args(name, args);
        let (args, serial) = match self.prepare_native(name, &args).await {
            Ok(prepared) => prepared,
            Err(error) => return (Err(error), None),
        };
        let result = self.dispatch(name, &args, dispatched).await;
        (result, serial)
    }
    async fn dispatch(
        &self,
        name: &str,
        args: &Value,
        dispatched: &mut bool,
    ) -> Result<Value, Error> {
        let driver = self.driver.get()?;
        match driver.execute_checked(name, &args).await {
            super::driver::NativeExecution::NotStarted(error) => Err(error),
            super::driver::NativeExecution::Dispatched(result) => {
                *dispatched = true;
                result
            }
        }
    }
    async fn prepare_native(
        &self,
        name: &str,
        args: &Value,
    ) -> Result<(Value, Option<MutationGuard>), Error> {
        if name == "harness_fork"
            && args.get("lastTurnId").is_some()
            && self.driver.get()?.descriptor()["capabilities"]["checkpointFork"] != true
        {
            return Err(Error::unsupported());
        }
        // Replies and interruption must pass a start/resume waiting on native approval.
        let descriptor = self.command_descriptor(name, args).await?;
        let query = descriptor
            .as_ref()
            .is_some_and(|row| row["execution"] == "query");
        let serial = if matches!(name, "harness_respond" | "harness_interrupt") || query {
            None
        } else {
            Some(self.mutations.acquire(name, args).await?)
        };
        let mut args = args.clone();
        if let Some(descriptor) = descriptor {
            super::commands::verify(&args, &descriptor)?;
            args["expectedCommand"] =
                json!({"id":descriptor["id"],"revision":descriptor["revision"]});
        }
        Ok((args, serial))
    }
    async fn command_descriptor(&self, name: &str, args: &Value) -> Result<Option<Value>, Error> {
        if name != "harness_command" {
            return Ok(None);
        }
        let catalog = self
            .driver
            .get()?
            .read("harness_commands", args.clone())
            .await?;
        catalog["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|row| row["name"] == args["command"])
            .cloned()
            .map(Some)
            .ok_or_else(Error::unsupported)
    }
}

async fn reconcile_receipt(
    receipts: &ReceiptScope,
    id: &str,
    drivers: Vec<Arc<dyn super::driver::HarnessDriver>>,
) -> Result<Value, Error> {
    let mut receipt = receipts.operation(id).await?;
    for driver in drivers {
        if receipt["outcome"] == "unknown" {
            if let Some(result) = driver.reconcile_operation(id).await? {
                receipt = persist_reconciliation(receipts, id, result).await?;
            }
        }
        driver.operation_settled(id, &receipt);
    }
    Ok(receipt)
}

async fn persist_reconciliation(
    receipts: &ReceiptScope,
    id: &str,
    native: Value,
) -> Result<Value, Error> {
    let started = std::time::Instant::now();
    let result = receipts
        .finish(
            id,
            json!({"outcome":"committed","requestId":id,"result":native}),
        )
        .await;
    let confirmed = result
        .as_ref()
        .is_ok_and(|receipt| receipt["outcome"] == "committed");
    crate::core::events::emit(
        if confirmed {
            crate::core::events::Level::Info
        } else {
            crate::core::events::Level::Warn
        },
        "harness.operation.reconcile",
        json!({"operation":"harness.operation.reconcile","operationId":id,"stage":"receipt_persistence",
            "outcome":if confirmed {"confirmed"} else {"unconfirmed"},"durationMs":started.elapsed().as_millis(),
            "code":result.as_ref().err().map(|error| error.code)}),
    );
    result
}

fn checkpoint_diagnostic(
    id: &str,
    args: &Value,
    result: &Result<Value, Error>,
    elapsed: std::time::Duration,
) {
    let verified = result
        .as_ref()
        .is_ok_and(|value| value["checkpoint"]["verified"] == true);
    let code = match result {
        Err(error) => Some(error.code),
        Ok(value) => value["checkpoint"]["code"].as_str().filter(|code| {
            code.len() <= 80
                && code
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        }),
    };
    let session = args["sessionId"]
        .as_str()
        .filter(|id| crate::core::ids::is_identifier_within(id, 256));
    crate::core::events::emit(
        if verified {
            crate::core::events::Level::Info
        } else {
            crate::core::events::Level::Warn
        },
        "harness.fork.result",
        json!({"operation":"harness.fork","operationId":id,"sessionId":session,
            "stage":"checkpoint","outcome":if verified {"verified"} else if result.is_err() {"rejected"} else {"unconfirmed"},
            "code":code,"durationMs":elapsed.as_millis()}),
    );
}

fn report_command_result(
    name: &str,
    id: &str,
    args: &Value,
    result: &Result<Value, Error>,
    dispatched: bool,
    started: std::time::Instant,
) {
    if name != "harness_command" {
        return;
    }
    let failed = result
        .as_ref()
        .map(|value| value["failed"] == true)
        .unwrap_or(true);
    crate::core::events::emit(
        if failed {
            crate::core::events::Level::Warn
        } else {
            crate::core::events::Level::Info
        },
        "harness.command.result",
        json!({"operation":"harness.command","operationId":id,"sessionId":args["sessionId"],
            "stage":if dispatched {"native"} else {"prepare"},"outcome":if failed {"failed"} else {"accepted"},
            "code":result.as_ref().err().map(|error|error.code),"durationMs":started.elapsed().as_millis()}),
    );
}

fn report_preparation_failure(
    id: &str,
    name: &str,
    result: &Result<Value, Error>,
    started: std::time::Instant,
) {
    if let Err(error) = result {
        crate::core::events::emit(
            crate::core::events::Level::Warn,
            "harness.operation.preparation_failed",
            json!({"operation":name,"operationId":id,"stage":"prepare","outcome":"not-committed",
                "code":error.code,"executionStarted":false,"durationMs":started.elapsed().as_millis()}),
        );
    }
}

fn receipt(id: &str, result: Result<Value, Error>, dispatched: bool) -> Value {
    match result {
        Ok(result) => json!({"outcome":"committed","requestId":id,"result":result}),
        // Read-only discovery and admission can fail with the same transport code
        // as a lost write response. Only a dispatched execution can be unknown.
        Err(error) if !dispatched => json!({"outcome":"not-committed","requestId":id,
            "code":error.code,"stage":"prepare","executionStarted":false}),
        Err(error) => {
            json!({"outcome":if error.code == "ETIMEDOUT" || error.code == "EIO" {"unknown"} else {"not-committed"},
                "requestId":id,"code":error.code})
        }
    }
}
fn fingerprint(name: &str, args: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            json!([name, execution_args(name, args)])
                .to_string()
                .as_bytes()
        )
    )
}
fn execution_args(name: &str, args: &Value) -> Value {
    let mut value = args.clone();
    if name == "harness_command" {
        if let Some(object) = value.as_object_mut() {
            object.remove("outputMode");
        }
    }
    value
}
fn output_mode(name: &str, args: &Value) -> Result<bool, Error> {
    let started = std::time::Instant::now();
    let result = if matches!(name, "harness_command" | "harness_operation") {
        super::command_outputs::reference_requested(args)
    } else {
        Ok(false)
    };
    if let Err(error) = result {
        let id = args["requestId"]
            .as_str()
            .filter(|id| crate::core::ids::is_identifier_within(id, 128));
        crate::core::events::emit(
            crate::core::events::Level::Warn,
            "harness.command.output_mode",
            json!({"operation":"harness.command.output_mode","operationId":id,"stage":"request-validation",
                "outcome":"failure","code":error.code,"durationMs":started.elapsed().as_millis()}),
        );
    }
    result
}

pub(super) fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128 * 1024)
        .ok_or_else(Error::invalid)
}

fn is_mutation(name: &str) -> bool {
    matches!(
        name,
        "harness_create"
            | "harness_resume"
            | "harness_fork"
            | "harness_rename"
            | "harness_archive"
            | "harness_unarchive"
            | "harness_delete"
            | "harness_turn"
            | "harness_command"
            | "harness_interrupt"
            | "harness_respond"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::driver::HarnessDriver;
    use futures_util::future::BoxFuture;
    use tokio::sync::Notify;

    #[test]
    fn output_mode_changes_only_command_presentation_not_other_mutation_identities() {
        let original = json!({"requestId":"same","sessionId":"native","command":"compact"});
        let mut referenced = original.clone();
        referenced["outputMode"] = json!("reference");
        assert_eq!(
            fingerprint("harness_command", &original),
            fingerprint("harness_command", &referenced)
        );
        assert_ne!(
            fingerprint("harness_turn", &original),
            fingerprint("harness_turn", &referenced)
        );
        assert_eq!(execution_args("harness_command", &referenced), original);
        for mode in [Value::Null, json!("raw"), json!("invalid"), json!(false)] {
            let mut invalid = original.clone();
            invalid["outputMode"] = mode;
            assert!(output_mode("harness_command", &invalid).is_err());
            assert!(output_mode("harness_operation", &invalid).is_err());
        }
    }

    struct SlowDrain {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }
    impl HarnessDriver for SlowDrain {
        fn descriptor(&self) -> Value {
            json!({})
        }
        fn read<'a>(&'a self, _: &'a str, _: Value) -> BoxFuture<'a, Result<Value, Error>> {
            Box::pin(async { Ok(json!({})) })
        }
        fn execute<'a>(&'a self, _: &'a str, _: &'a Value) -> BoxFuture<'a, Result<Value, Error>> {
            Box::pin(async { Ok(json!({})) })
        }
        fn stop(&self) {}
        fn drain(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {
                self.entered.notify_one();
                self.release.notified().await;
            })
        }
    }

    #[tokio::test]
    async fn draining_a_project_does_not_hold_the_registry_lock() {
        let harness = Arc::new(Harnesses::new(Vec::new(), "epoch".into()).unwrap());
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        harness.scoped.lock().await.insert(
            "slow:1:codex:false".into(),
            Arc::new(Profile {
                driver: DriverSlot::testing(Box::new(SlowDrain {
                    entered: entered.clone(),
                    release: release.clone(),
                })),
                receipts: harness.receipts.scoped("slow-test".into()),
                mutations: MutationQueues::default(),
            }),
        );
        let closing = harness.clone();
        let work = tokio::spawn(async move {
            closing.close_project("slow").await;
        });
        entered.notified().await;
        let available =
            tokio::time::timeout(std::time::Duration::from_secs(1), harness.scoped.lock()).await;
        // Release cleanup before asserting so a regression cannot strand the test task.
        release.notify_one();
        let registry = available.expect("unrelated project requests must not wait for shutdown");
        assert!(registry.contains_key("slow:1:codex:false"));
        drop(registry);
        work.await.unwrap();
        assert!(harness.scoped.lock().await.is_empty());
    }
}
