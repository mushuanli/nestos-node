use crate::{app::State, plugin::files::http as handlers, plugins::NodePlugin};
use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use std::sync::Arc;

pub struct Files;
impl NodePlugin for Files {
    fn capabilities(
        &self,
        state: &State,
        identity: usize,
        enabled: bool,
    ) -> crate::plugin::common::Capabilities {
        super::contribution::capabilities(state, identity, enabled)
    }
    fn directory_fields(&self) -> &'static [&'static str] {
        &["exports"]
    }
    fn directories(
        &self,
        state: &State,
        identity: usize,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Map<String, serde_json::Value>, crate::core::error::Error> {
        Ok(crate::plugin::common::object(
            serde_json::json!({"exports":super::directories::exports(state,state.auth.client(identity))}),
        ))
    }
    fn id(&self) -> &'static str {
        "files"
    }
    fn configure(
        &self,
        _config: &dyn crate::plugin::common::PluginConfiguration,
        state: &mut State,
    ) -> Result<(), String> {
        state
            .services
            .insert(Arc::new(super::operations::Operations::default()))
    }
    fn stop<'a>(&'a self, state: &'a Arc<State>) -> crate::plugin::common::PluginFuture<'a> {
        Box::pin(async move {
            if let Some(service) = state.services.get::<super::operations::Operations>() {
                service.cancel_all();
            }
            Ok(())
        })
    }
    fn routes(&self) -> Router<Arc<State>> {
        Router::new()
            .route("/v1/exports", get(handlers::exports::exports))
            .route("/v1/fs/:alias/stat", post(handlers::stat::stat))
            .route("/v1/fs/:alias/entries", get(handlers::entries::entries))
            .route(
                "/v1/fs/:alias/content",
                get(handlers::content::content).put(handlers::mutations::replace),
            )
            .route("/v1/fs/:alias/mutate", post(handlers::mutations::mutate))
            .merge(seq_routes())
            .merge(operation_routes())
            .layer(DefaultBodyLimit::max(512 * 1024))
    }
}

fn seq_routes() -> Router<Arc<State>> {
    Router::new()
        .route("/v1/fs/:alias/seq/snapshot", post(handlers::seq::snapshot))
        .route("/v1/fs/:alias/seq/query", post(handlers::seq::query))
        .route("/v1/fs/:alias/seq/body", post(handlers::seq::body))
        .route("/v1/fs/:alias/seq/body/put", post(handlers::seq::put_body))
        .route(
            "/v1/fs/:alias/seq/transaction",
            post(handlers::seq::transaction),
        )
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
}
fn operation_routes() -> Router<Arc<State>> {
    Router::new()
        .route(
            "/v1/fs/:alias/operations/:id",
            get(handlers::operations::status),
        )
        .route(
            "/v1/fs/:alias/operations/:id/cancel",
            post(handlers::operations::cancel),
        )
}
