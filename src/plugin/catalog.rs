//! Product assembly; the common registry never imports concrete features.
use crate::{
    app::State,
    config::Config,
    plugin::common::{NodePlugin, PluginRegistry},
};
use std::sync::Arc;
impl PluginRegistry {
    pub fn builtins() -> Self {
        Self::with_harness(crate::harness::HarnessPlugins::default())
    }
    pub(crate) fn with_harness(harness: crate::harness::HarnessPlugins) -> Self {
        let mut registry = Self::empty();
        for plugin in [
            Arc::new(super::files::plugin::Files) as Arc<dyn NodePlugin>,
            Arc::new(super::process::plugin::Process),
            Arc::new(super::sync::plugin::Sync),
            Arc::new(super::projects::plugin::Projects),
            Arc::new(super::harness::plugin::Harness(harness)),
            Arc::new(super::mcp::plugin::Mcp),
        ] {
            registry.register(plugin).expect("Unique built-in plugin");
        }
        registry
    }
}
impl State {
    pub fn from_config(config: &Config) -> Result<Arc<Self>, String> {
        Self::from_config_with_registry(config, PluginRegistry::builtins())
    }
    pub fn from_config_with_plugins(
        config: &Config,
        plugins: crate::harness::HarnessPlugins,
    ) -> Result<Arc<Self>, String> {
        Self::from_config_with_registry(config, PluginRegistry::with_harness(plugins))
    }
    pub fn from_config_with_registry(
        config: &Config,
        registry: PluginRegistry,
    ) -> Result<Arc<Self>, String> {
        let (effective, plan) = registry.resolve(config)?;
        let config = crate::config::plugin_adapter::deployment(effective.as_ref())?;
        crate::config::identity::validate_identity(&config)?;
        let exports = configured_exports(&config)?;
        let auth = configured_auth(&config, &exports)?;
        let mut state = Self::bare(
            auth,
            exports,
            crate::app::random_bytes()?,
            crate::app::random_epoch()?,
        );
        plan.configure(effective.as_ref(), &mut state)?;
        state.plugins = plan;
        state.allowed_origins = config.allowed_origins.clone();
        Ok(Arc::new(state))
    }
}
pub(crate) fn compatibility(state: &mut State) -> Result<(), String> {
    state.plugins = PluginRegistry::builtins().all_enabled()?;
    state
        .services
        .insert(Arc::new(crate::operations::Operations::default()))?;
    state
        .services
        .insert(Arc::new(crate::process::Execution::new(
            state.epoch.clone(),
        )))?;
    state
        .services
        .insert(Arc::new(crate::harness::Harnesses::new(
            vec![],
            state.epoch.clone(),
        )?))
}

use crate::{auth::Auth, fs::Exports};
pub(crate) fn configured_exports(config: &crate::config::Config) -> Result<Exports, String> {
    crate::config::exports::validate_presence(config)?;
    if config.exports.is_empty() {
        Ok(Exports::new(std::collections::BTreeMap::new()))
    } else {
        crate::config::open_exports(&config.exports)
    }
}
pub(crate) fn configured_auth(
    config: &crate::config::Config,
    exports: &Exports,
) -> Result<Auth, String> {
    let server_id = match &config.server_id {
        Some(id) => Some(id.clone()),
        None if config.execution || !config.harnesses.is_empty() => {
            Some(format!("cells-{}", crate::app::random_epoch()?))
        }
        None => None,
    };
    crate::config::authorization::compile(config, exports, server_id)
}
