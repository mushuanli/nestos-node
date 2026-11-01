//! HTTP transport.
//!
//! The router is the only place that knows the URL layout, the only place that
//! decides CORS and body limits, and it delegates every request to a handler
//! that extracts input and calls one domain operation. Handlers never reach
//! into filesystem or process internals directly.

pub mod access;
mod events;
pub mod handlers;
pub mod query;
pub mod range;
pub(crate) mod subscriptions;

use crate::app::State;
use axum::{
    http::{HeaderName, HeaderValue, Method},
    middleware,
    routing::get,
    Router,
};
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Headers a browser client may send; anything else fails the CORS preflight.
const ALLOWED_HEADERS: [&str; 10] = [
    "x-cells-principal",
    "x-cells-grant-revision",
    "authorization",
    "content-type",
    "range",
    "if-range",
    "if-match",
    "if-none-match",
    "x-timeout-ms",
    "x-operation-id",
];

pub fn router(state: Arc<State>, origins: &[String]) -> Result<Router, String> {
    let allowed = ALLOWED_HEADERS
        .into_iter()
        .chain(state.plugins.request_headers())
        .map(|name| {
            name.parse::<HeaderName>()
                .map_err(|_| "Invalid plugin request header".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let cors = CorsLayer::new()
        .allow_origin(allow_origin(origins)?)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::OPTIONS])
        .allow_headers(allowed)
        .expose_headers([
            HeaderName::from_static("content-range"),
            HeaderName::from_static("etag"),
        ]);
    Ok(Router::new()
        .route(
            "/v1/capabilities",
            get(handlers::capabilities::capabilities),
        )
        .merge(state.plugins.routes())
        .merge(subscriptions::routes())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            access::authenticate,
        ))
        .layer(cors)
        // File content and metadata must never be cached by an intermediary.
        .layer(middleware::map_response(
            |mut response: axum::response::Response| async move {
                response
                    .headers_mut()
                    .insert("cache-control", HeaderValue::from_static("no-store"));
                response
            },
        ))
        .layer(middleware::from_fn(events::failures))
        .with_state(state))
}

/// `["*"]` allows every browser origin; otherwise entries must match exactly.
fn allow_origin(origins: &[String]) -> Result<AllowOrigin, String> {
    if origins.iter().any(|origin| origin == "*") {
        return Ok(AllowOrigin::any());
    }
    let values = origins
        .iter()
        .map(|value| {
            value
                .parse::<HeaderValue>()
                .map_err(|_| format!("Invalid CORS origin {value:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AllowOrigin::list(values))
}

#[cfg(test)]
mod tests {
    use super::allow_origin;

    #[test]
    fn wildcard_and_exact_origins_are_accepted() {
        assert!(allow_origin(&["*".to_owned()]).is_ok());
        assert!(allow_origin(&["*".to_owned(), "http://localhost:3000".to_owned()]).is_ok());
        assert!(allow_origin(&["http://localhost:3000".to_owned()]).is_ok());
        assert!(allow_origin(&[]).is_ok());
        assert!(allow_origin(&["http://bad\norigin".to_owned()]).is_err());
    }
}
