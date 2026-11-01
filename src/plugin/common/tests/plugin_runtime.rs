#[path = "support/mod.rs"]
mod common;
use cells::{
    app::State,
    config::Config,
    fs::Export,
    plugins::{NodePlugin, PluginFuture, PluginRegistry, PluginRuntime},
};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Feature {
    id: &'static str,
    dependencies: &'static [&'static str],
    events: Arc<Mutex<Vec<String>>>,
    fail_start: bool,
    fail_stop: bool,
    block: Option<Arc<tokio::sync::Notify>>,
}
impl Feature {
    fn new(id: &'static str, events: &Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            id,
            dependencies: &[],
            events: events.clone(),
            fail_start: false,
            fail_stop: false,
            block: None,
        }
    }
}
impl NodePlugin for Feature {
    fn id(&self) -> &'static str {
        self.id
    }
    fn dependencies(&self) -> &'static [&'static str] {
        self.dependencies
    }
    fn start<'a>(&'a self, _state: &'a Arc<State>) -> PluginFuture<'a> {
        Box::pin(async move {
            self.events
                .lock()
                .unwrap()
                .push(format!("start:{}", self.id));
            if let Some(notify) = &self.block {
                notify.notify_one();
                std::future::pending::<()>().await;
            }
            if self.fail_start {
                Err("fixture startup".into())
            } else {
                Ok(())
            }
        })
    }
    fn stop<'a>(&'a self, _state: &'a Arc<State>) -> PluginFuture<'a> {
        Box::pin(async move {
            self.events
                .lock()
                .unwrap()
                .push(format!("stop:{}", self.id));
            if self.fail_stop {
                Err("fixture shutdown".into())
            } else {
                Ok(())
            }
        })
    }
}
fn config() -> Config {
    toml::from_str("listen='127.0.0.1:0'\nexecution=false").unwrap()
}
fn state(root: &std::path::Path) -> Arc<State> {
    common::state(
        Export::open(root).unwrap(),
        None,
        vec!["docs".into()],
        vec![],
    )
}

#[test]
fn validates_registration_dependencies_cycles_and_disabled_dependencies() {
    let events = Arc::new(Mutex::new(vec![]));
    let mut registry = PluginRegistry::empty();
    let mut a = Feature::new("a", &events);
    a.dependencies = &["b"];
    registry.register(Arc::new(a.clone())).unwrap();
    assert!(registry.register(Arc::new(a.clone())).is_err());
    assert!(registry
        .resolve(&config())
        .err()
        .unwrap()
        .contains("requires enabled"));
    let mut b = Feature::new("b", &events);
    registry.register(Arc::new(b.clone())).unwrap();
    assert_eq!(registry.resolve(&config()).unwrap().1.order(), ["b", "a"]);
    let mut disabled = config();
    disabled.plugins.insert("b".into(), false);
    assert!(registry.resolve(&disabled).is_err());
    let mut cycle = PluginRegistry::empty();
    b.dependencies = &["a"];
    cycle.register(Arc::new(a)).unwrap();
    cycle.register(Arc::new(b)).unwrap();
    assert!(cycle.resolve(&config()).err().unwrap().contains("cycle"));
}

#[tokio::test]
async fn unwinds_partial_start_in_reverse_order_and_preserves_both_failures() {
    let events = Arc::new(Mutex::new(vec![]));
    let mut registry = PluginRegistry::empty();
    let mut a = Feature::new("a", &events);
    a.fail_stop = true;
    let mut b = Feature::new("b", &events);
    b.fail_start = true;
    for feature in [a, b, Feature::new("c", &events)] {
        registry.register(Arc::new(feature)).unwrap();
    }
    let plan = registry.resolve(&config()).unwrap().1;
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let runtime = PluginRuntime::default();
    let error = runtime.start(&plan, &state).await.unwrap_err();
    assert!(error.contains("fixture startup") && error.contains("fixture shutdown"));
    assert_eq!(
        *events.lock().unwrap(),
        ["start:a", "start:b", "stop:b", "stop:a"]
    );
    runtime.stop(&state).await.unwrap();
    assert!(runtime.start(&plan, &state).await.is_err());
}

#[tokio::test]
async fn repeats_start_and_shutdown_safely_without_reinvoking_hooks() {
    let events = Arc::new(Mutex::new(vec![]));
    let mut registry = PluginRegistry::empty();
    registry
        .register(Arc::new(Feature::new("a", &events)))
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let plan = registry.resolve(&config()).unwrap().1;
    let runtime = PluginRuntime::default();
    runtime.start(&plan, &state).await.unwrap();
    runtime.start(&plan, &state).await.unwrap();
    runtime.stop(&state).await.unwrap();
    runtime.stop(&state).await.unwrap();
    assert_eq!(*events.lock().unwrap(), ["start:a", "stop:a"]);
}

#[tokio::test]
async fn retains_partial_start_cleanup_when_the_start_future_is_cancelled() {
    let events = Arc::new(Mutex::new(vec![]));
    let notify = Arc::new(tokio::sync::Notify::new());
    let mut feature = Feature::new("a", &events);
    feature.block = Some(notify.clone());
    let mut registry = PluginRegistry::empty();
    registry.register(Arc::new(feature)).unwrap();
    let plan = registry.resolve(&config()).unwrap().1;
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let runtime = Arc::new(PluginRuntime::default());
    let (running, context) = (runtime.clone(), state.clone());
    let task = tokio::spawn(async move { running.start(&plan, &context).await });
    notify.notified().await;
    task.abort();
    let _ = task.await;
    runtime.stop(&state).await.unwrap();
    assert_eq!(*events.lock().unwrap(), ["start:a", "stop:a"]);
}

struct BlockingStop {
    feature: Feature,
    notify: Arc<tokio::sync::Notify>,
    first: std::sync::atomic::AtomicBool,
}
impl NodePlugin for BlockingStop {
    fn id(&self) -> &'static str {
        self.feature.id
    }
    fn start<'a>(&'a self, state: &'a Arc<State>) -> PluginFuture<'a> {
        self.feature.start(state)
    }
    fn stop<'a>(&'a self, state: &'a Arc<State>) -> PluginFuture<'a> {
        Box::pin(async move {
            if self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.notify.notify_one();
                std::future::pending::<()>().await;
            }
            self.feature.stop(state).await
        })
    }
}
#[tokio::test]
async fn retains_remaining_cleanup_when_the_shutdown_future_is_cancelled() {
    let events = Arc::new(Mutex::new(vec![]));
    let notify = Arc::new(tokio::sync::Notify::new());
    let mut registry = PluginRegistry::empty();
    registry
        .register(Arc::new(Feature::new("a", &events)))
        .unwrap();
    registry
        .register(Arc::new(BlockingStop {
            feature: Feature::new("b", &events),
            notify: notify.clone(),
            first: true.into(),
        }))
        .unwrap();
    let plan = registry.resolve(&config()).unwrap().1;
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let runtime = Arc::new(PluginRuntime::default());
    runtime.start(&plan, &state).await.unwrap();
    let (running, context) = (runtime.clone(), state.clone());
    let task = tokio::spawn(async move { running.stop(&context).await });
    notify.notified().await;
    task.abort();
    let _ = task.await;
    runtime.stop(&state).await.unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        ["start:a", "start:b", "stop:b", "stop:a"]
    );
}
