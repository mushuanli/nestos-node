//! A second plugin proves dispatch, project scopes and receipts are not Codex-specific.
use futures_util::future::BoxFuture;
use cells::projects::runtime::ProjectRuntime;
use cells::{
    app::State,
    config::Config,
    core::error::Error,
    harness::{config::ProfileConfig, HarnessDriver, HarnessPlugin, HarnessPlugins},
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
#[path = "command_outputs.rs"]
mod command_outputs;

struct FixturePlugin {
    executions: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}
struct FixtureDriver {
    profile: ProfileConfig,
    scope: Option<String>,
    executions: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

impl HarnessPlugin for FixturePlugin {
    fn kind(&self) -> &'static str {
        "fixture"
    }
    fn validate(&self, config: &ProfileConfig) -> Result<(), String> {
        if config.command == "fixture" {
            Ok(())
        } else {
            Err("Fixture command required".into())
        }
    }
    fn create(
        &self,
        profile: ProfileConfig,
        runtime: Option<Arc<ProjectRuntime>>,
    ) -> Result<Box<dyn HarnessDriver>, String> {
        Ok(Box::new(FixtureDriver {
            profile,
            scope: runtime.map(|r| r.project.id.clone()),
            executions: self.executions.clone(),
            stops: self.stops.clone(),
        }))
    }
}
impl HarnessDriver for FixtureDriver {
    fn descriptor(&self) -> Value {
        json!({"id":self.profile.id,"kind":"fixture","projectRuntime":self.profile.projects})
    }
    fn read<'a>(&'a self, name: &'a str, _args: Value) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            if name == "harness_commands" {
                return Ok(
                    json!({"commands":[{"name":"compact","id":"fixture-compact","revision":"1","execution":"serial","kind":"action"}]}),
                );
            }
            Ok(json!({"sessions":[],"nextCursor":null,"scope":self.scope}))
        })
    }
    fn execute<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            self.executions.fetch_add(1, Ordering::SeqCst);
            if name == "harness_command" {
                assert!(args.get("outputMode").is_none());
                let output = if args["arguments"] == "large" {
                    "界😀".repeat(40_000)
                } else {
                    "small fixture output".into()
                };
                return Ok(
                    json!({"commandId":args["requestId"],"output":output,"status":"completed"}),
                );
            }
            Ok(json!({"turnId":"fixture-turn"}))
        })
    }
    fn stop(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
    fn drain(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

fn fixture() -> (
    tempfile::TempDir,
    Config,
    HarnessPlugins,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let root = tempfile::tempdir().unwrap();
    for name in ["home", "work"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    let config = toml::from_str(&format!(
        "listen='127.0.0.1:0'\nserver_id='plugin-fixture'\nexecution=false\napi_key='fixture-api-key-at-least-24-bytes'\n[projects]\nroot='{}'\n[[exports]]\nalias='work'\npath='{}'\naccess='rw'\n[[harnesses]]\nid='custom'\nkind='fixture'\ncommand='fixture'\nhome='{}'\nprojects=true\n",
        root.path().join("catalog").display(),root.path().join("work").display(),root.path().join("home").display())).unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let mut plugins = HarnessPlugins::default();
    plugins
        .register(Arc::new(FixturePlugin {
            executions: executions.clone(),
            stops: stops.clone(),
        }))
        .unwrap();
    (root, config, plugins, executions, stops)
}

#[tokio::test]
async fn registered_plugins_dispatch_with_shared_receipts_and_shutdown() {
    let (_root, config, plugins, executions, stops) = fixture();
    let state = State::from_config_with_plugins(&config, plugins).unwrap();
    let profiles = state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .profiles();
    assert_eq!(profiles["profiles"][0]["kind"], "fixture");
    let args = json!({"profileId":"custom","epoch":profiles["epoch"],"requestId":"once"});
    let first = state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .call("harness_turn", args.clone())
        .await
        .unwrap();
    assert_eq!(
        state
            .services
            .require::<cells::harness::Harnesses>()
            .unwrap()
            .call("harness_turn", args.clone())
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        state
            .services
            .require::<cells::harness::Harnesses>()
            .unwrap()
            .call("harness_operation", args)
            .await
            .unwrap(),
        first
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert!(state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .call("harness_sessions", json!({"profileId":"custom"}))
        .await
        .is_err());
}

#[tokio::test]
async fn profile_aliases_share_the_driver_and_canonical_receipts() {
    let (_root, mut config, plugins, executions, _) = fixture();
    config.harnesses[0].aliases = vec!["custom-old".into()];
    let state = State::from_config_with_plugins(&config, plugins).unwrap();
    let profiles = state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .profiles();
    assert_eq!(profiles["profiles"].as_array().unwrap().len(), 1);
    assert_eq!(profiles["profiles"][0]["aliases"], json!(["custom-old"]));
    let mut args = json!({"profileId":"custom-old","epoch":profiles["epoch"],"requestId":"once"});
    let first = state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .call("harness_turn", args.clone())
        .await
        .unwrap();
    args["profileId"] = json!("custom");
    assert_eq!(
        state
            .services
            .require::<cells::harness::Harnesses>()
            .unwrap()
            .call("harness_turn", args.clone())
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        state
            .services
            .require::<cells::harness::Harnesses>()
            .unwrap()
            .call("harness_operation", args)
            .await
            .unwrap(),
        first
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
}

#[test]
fn profile_aliases_cannot_shadow_ids_or_other_aliases() {
    let (_root, mut config, plugins, _, _) = fixture();
    for aliases in [vec!["custom"], vec!["old", "old"], vec!["../bad"]] {
        config.harnesses[0].aliases = aliases.into_iter().map(String::from).collect();
        assert!(cells::harness::config::validate_with_plugins(&config, &plugins).is_err());
    }
    config.harnesses[0].aliases = vec!["other".into()];
    let mut second = config.harnesses[0].clone();
    second.id = "other".into();
    second.aliases.clear();
    config.harnesses.push(second);
    assert!(cells::harness::config::validate_with_plugins(&config, &plugins).is_err());
}

#[tokio::test]
async fn project_scoped_driver_uses_the_registered_plugin() {
    let (_root, config, plugins, _executions, stops) = fixture();
    let state = State::from_config_with_plugins(&config, plugins).unwrap();
    let registered = cells::projects::call(
        &state,
        0,
        "project_register",
        json!({"name":"Work","alias":"work","path":"","access":"rw"}),
    )
    .await
    .unwrap();
    let project = &registered["project"];
    let page = state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .project_call(
            &state,
            0,
            "harness_sessions",
            json!({"profileId":"custom","projectId":project["id"],"revision":project["revision"]}),
        )
        .await
        .unwrap();
    assert_eq!(page["scope"], project["id"]);
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close_project(project["id"].as_str().unwrap())
        .await;
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
    assert_eq!(stops.load(Ordering::SeqCst), 2);
}

#[test]
fn validation_rejects_missing_duplicate_and_invalid_plugin_configuration() {
    let (_root, mut config, mut plugins, executions, stops) = fixture();
    assert!(cells::harness::config::validate(&config).is_err());
    assert!(cells::harness::config::validate_with_plugins(&config, &plugins).is_ok());
    assert!(plugins
        .register(Arc::new(FixturePlugin { executions, stops }))
        .is_err());
    config.harnesses[0].command = "unexpected".into();
    assert!(cells::harness::config::validate_with_plugins(&config, &plugins).is_err());
}
