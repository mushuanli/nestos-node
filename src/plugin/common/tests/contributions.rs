#[path = "support/mod.rs"]
mod common;
use axum::{body::Body, extract::State as AxumState, routing::get, Json, Router};
use cells::{
    app::{Application, State},
    auth::{Auth, Client},
    fs::Exports,
    plugin::common::{
        object, Capabilities, McpContribution, NodePlugin, PluginConfiguration, PluginRegistry,
        PluginSettings, ToolFuture,
    },
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock},
};
use tower::ServiceExt;
struct Feature {
    id: &'static str,
}
struct OwnState {
    value: String,
}
static TOOLS: LazyLock<Vec<Value>> = LazyLock::new(|| {
    vec![
        json!({"name":"fixture_echo","description":"Fixture only","inputSchema":{"type":"object"}}),
    ]
});
impl NodePlugin for Feature {
    fn id(&self) -> &'static str {
        self.id
    }
    fn mcp(&self) -> Option<&dyn McpContribution> {
        Some(self)
    }
    fn configure(&self, config: &dyn PluginConfiguration, host: &mut State) -> Result<(), String> {
        let value = config
            .option(self.id)
            .and_then(|v| v.get("value"))
            .and_then(|v| v.as_str())
            .ok_or("Missing fixture value")?;
        host.services.insert(Arc::new(OwnState {
            value: value.into(),
        }))
    }
    fn capabilities(&self, _state: &State, _identity: usize, enabled: bool) -> Capabilities {
        Capabilities {
            http: object(json!({"fixture":enabled})),
            mcp: object(json!({"fixture":enabled})),
        }
    }
    fn routes(&self) -> Router<Arc<State>> {
        Router::new().route(
            "/v1/fixture",
            get(|AxumState(state): AxumState<Arc<State>>| async move {
                let value = state.services.require::<OwnState>()?;
                Ok::<_, cells::core::error::Error>(Json(json!({"value":value.value})))
            }),
        )
    }
}
impl McpContribution for Feature {
    fn tools(&self) -> &[Value] {
        &TOOLS
    }
    fn call<'a>(
        &'a self,
        state: &'a Arc<State>,
        _identity: usize,
        _name: &'a str,
        _args: Value,
    ) -> ToolFuture<'a> {
        Box::pin(async move { Ok(json!({"value":state.services.require::<OwnState>()?.value})) })
    }
    fn resources(&self, _state: &State, _identity: usize) -> Vec<Value> {
        vec![json!({"uri":"fixture://value","name":"value"})]
    }
    fn read<'a>(
        &'a self,
        state: &'a Arc<State>,
        _identity: usize,
        uri: &'a str,
        _deadline: tokio::time::Instant,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            Ok(
                json!({"resultType":"complete","contents":[{"uri":uri,"text":state.services.require::<OwnState>()?.value}]}),
            )
        })
    }
}
fn build(enabled: bool) -> Application {
    let auth = Auth::new(
        Some("fixture".into()),
        vec![Client::new(common::SECRET.into(), None, vec![], vec![])],
    );
    let host = State::host(auth, Exports::new(BTreeMap::new())).unwrap();
    let mut registry = PluginRegistry::empty();
    registry
        .register(Arc::new(Feature { id: "fixture" }))
        .unwrap();
    registry
        .register(Arc::new(cells::plugin::mcp::Plugin))
        .unwrap();
    let config = PluginSettings {
        enabled: BTreeMap::from([("fixture".into(), enabled)]),
        options: BTreeMap::from([(
            "fixture".into(),
            toml::from_str("value='configured'").unwrap(),
        )]),
    };
    Application::assemble(host, registry, &config).unwrap()
}
async fn rpc(app: &Application, method: &str, mut params: Value) -> Value {
    params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}});
    let mut request = common::request("/mcp")
        .method("POST")
        .header("content-type", "application/json")
        .header("mcp-method", method)
        .header("mcp-protocol-version", "2026-07-28");
    if let Some(name) = params["name"].as_str().or(params["uri"].as_str()) {
        request = request.header("mcp-name", name);
    }
    let response = app
        .router()
        .unwrap()
        .oneshot(
            request
                .body(Body::from(
                    json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = common::body_json(response).await;
    assert!(body.get("error").is_none(), "{body}");
    body["result"].clone()
}
#[tokio::test]
async fn independently_assembles_own_configuration_state_routes_tools_resources_and_capabilities() {
    let app = build(true);
    app.start().await.unwrap();
    assert!(app
        .state()
        .services
        .get::<cells::harness::Harnesses>()
        .is_none());
    assert!(app
        .state()
        .services
        .get::<cells::process::Execution>()
        .is_none());
    let response = app
        .router()
        .unwrap()
        .oneshot(common::request("/v1/fixture").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(common::body_json(response).await["value"], "configured");
    assert_eq!(
        rpc(&app, "tools/list", json!({})).await["tools"][0]["name"],
        "fixture_echo"
    );
    assert_eq!(
        rpc(&app, "tools/call", json!({"name":"fixture_echo"})).await["structuredContent"]["value"],
        "configured"
    );
    assert_eq!(
        rpc(&app, "resources/list", json!({})).await["resources"][0]["uri"],
        "fixture://value"
    );
    assert_eq!(
        rpc(&app, "resources/read", json!({"uri":"fixture://value"})).await["contents"][0]["text"],
        "configured"
    );
    assert_eq!(
        rpc(&app, "server/discover", json!({})).await["_meta"]["itookit/cells"]["fixture"],
        true
    );
    app.shutdown().await.unwrap();
}
#[tokio::test]
async fn switching_off_a_plugin_removes_its_state_routes_tools_and_resources() {
    let app = build(false);
    app.start().await.unwrap();
    assert!(app.state().services.get::<OwnState>().is_none());
    assert!(app
        .state()
        .services
        .get::<cells::operations::Operations>()
        .is_none());
    assert_eq!(
        app.router()
            .unwrap()
            .oneshot(common::request("/v1/fixture").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        404
    );
    assert!(rpc(&app, "tools/list", json!({})).await["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t["name"] != "fixture_echo"));
    let list = rpc(&app, "resources/list", json!({})).await;
    assert!(list["resources"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["uri"] != "fixture://value"));
    app.shutdown().await.unwrap();
}
#[test]
fn duplicate_tool_declarations_are_rejected_before_service_construction() {
    let mut registry = PluginRegistry::empty();
    registry.register(Arc::new(Feature { id: "one" })).unwrap();
    registry.register(Arc::new(Feature { id: "two" })).unwrap();
    assert!(registry
        .resolve(&PluginSettings::default())
        .err()
        .unwrap()
        .contains("Duplicate plugin tool"));
}

struct ConflictingResource {
    id: &'static str,
}
impl NodePlugin for ConflictingResource {
    fn id(&self) -> &'static str {
        self.id
    }
    fn mcp(&self) -> Option<&dyn McpContribution> {
        Some(self)
    }
}
impl McpContribution for ConflictingResource {
    fn resources(&self, _state: &State, _identity: usize) -> Vec<Value> {
        vec![json!({"uri":"fixture://ambiguous","name":"ambiguous"})]
    }
}
#[test]
fn resource_conflicts_fail_closed_for_discovery_and_reads() {
    let auth = Auth::new(
        Some("fixture".into()),
        vec![Client::new(common::SECRET.into(), None, vec![], vec![])],
    );
    let host = State::host(auth, Exports::new(BTreeMap::new())).unwrap();
    let mut registry = PluginRegistry::empty();
    registry
        .register(Arc::new(ConflictingResource { id: "a" }))
        .unwrap();
    registry
        .register(Arc::new(ConflictingResource { id: "b" }))
        .unwrap();
    let app = Application::assemble(host, registry, &PluginSettings::default()).unwrap();
    let state = app.state();
    assert_eq!(
        state.plugins.resources(state, 0).unwrap_err().code,
        "PLUGIN_RESOURCE_CONFLICT"
    );
    assert_eq!(
        state
            .plugins
            .resource(state, 0, "fixture://ambiguous")
            .err()
            .unwrap()
            .code,
        "PLUGIN_RESOURCE_CONFLICT"
    );
}

struct Prepared {
    closed: Arc<std::sync::atomic::AtomicUsize>,
}
impl NodePlugin for Prepared {
    fn id(&self) -> &'static str {
        "prepared"
    }
    fn stop<'a>(&'a self, _state: &'a Arc<State>) -> cells::plugin::common::PluginFuture<'a> {
        Box::pin(async move {
            self.closed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
    }
}
#[tokio::test]
async fn shutdown_releases_prepared_plugins_even_before_the_first_start() {
    let closed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = PluginRegistry::empty();
    registry
        .register(Arc::new(Prepared {
            closed: closed.clone(),
        }))
        .unwrap();
    let auth = Auth::new(
        Some("fixture".into()),
        vec![Client::new(common::SECRET.into(), None, vec![], vec![])],
    );
    let host = State::host(auth, Exports::new(BTreeMap::new())).unwrap();
    let app = Application::assemble(host, registry, &PluginSettings::default()).unwrap();
    app.shutdown().await.unwrap();
    app.shutdown().await.unwrap();
    assert_eq!(closed.load(std::sync::atomic::Ordering::SeqCst), 1);
}
