//! Request-boundary policy: identity, export authorization and deadlines.
//!
//! Every handler goes through these helpers, so the rules "authentication is
//! required", "the alias must be authorized" and "every request has a bounded
//! budget" are stated exactly once.

use crate::{app::State, core::error::Error, fs::Export};
use axum::http::HeaderMap;
use std::{sync::Arc, time::Duration};

/// Plugin routes share authentication; each handler still enforces its resource grants.
pub(crate) async fn authenticate(
    axum::extract::State(state): axum::extract::State<Arc<State>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, Error> {
    identity(&state, request.headers())?;
    Ok(next.run(request).await)
}

/// Budget used when the caller does not ask for one.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Hard ceiling on a caller-supplied budget.
const MAX_TIMEOUT_MS: u64 = 30_000;

/// Resolve the request identity.
pub fn identity(state: &State, headers: &HeaderMap) -> Result<usize, Error> {
    let identity = state.auth.identify(headers).inspect_err(|error| {
        crate::core::events::emit(
            crate::core::events::Level::Warn,
            "auth.rejected",
            serde_json::json!({"code": error.code}),
        );
    })?;
    let authorization = state.auth.client(identity).authorization();
    for (header, field) in [
        ("x-cells-principal", "principalId"),
        ("x-cells-grant-revision", "revision"),
    ] {
        if headers
            .get(header)
            .is_some_and(|value| value.to_str().ok() != authorization[field].as_str())
        {
            return Err(Error::forbidden("AUTHORIZATION_CHANGED"));
        }
    }
    Ok(identity)
}

pub fn origin(state: &State, headers: &HeaderMap) -> Result<(), Error> {
    let Some(origin) = headers.get("origin") else {
        return Ok(());
    };
    let origin = origin.to_str().map_err(|_| Error::forbidden("EACCES"))?;
    if state
        .allowed_origins
        .iter()
        .any(|allowed| allowed == origin || allowed == "*")
    {
        Ok(())
    } else {
        Err(Error::forbidden("EACCES"))
    }
}

/// Resolve the identity and prove it may read `alias`.
pub fn export(
    state: &State,
    headers: &HeaderMap,
    alias: &str,
) -> Result<(usize, Arc<Export>), Error> {
    let identity = identity(state, headers)?;
    state.auth.authorize(identity, alias).inspect_err(|_| {
        crate::core::events::emit(crate::core::events::Level::Warn,"authorization.rejected",
            serde_json::json!({"principalId":state.auth.client(identity).id(),"operation":"files.read","alias":alias.chars().take(128).collect::<String>()}));
    })?;
    let export = state
        .exports
        .get(alias)
        .ok_or_else(|| Error::forbidden("EACCES"))?
        .clone();
    Ok((identity, export))
}

/// Like [`export`], but the caller must also be allowed to write the alias.
pub fn writable_export(
    state: &State,
    headers: &HeaderMap,
    alias: &str,
) -> Result<(usize, Arc<Export>), Error> {
    let (identity, export) = export(state, headers, alias)?;
    if !state.auth.client(identity).may_write(alias) || !export.writable() {
        crate::core::events::emit(
            crate::core::events::Level::Warn,
            "authorization.rejected",
            serde_json::json!({"principalId":state.auth.client(identity).id(),"operation":"files.write","alias":alias}),
        );
        return Err(Error::forbidden("EROFS"));
    }
    Ok((identity, export))
}

/// The idempotency key every write must carry.
pub fn operation_id(headers: &HeaderMap) -> Result<&str, Error> {
    headers
        .get("x-operation-id")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(Error::invalid)
}

/// The absolute instant this request must finish by.
pub fn deadline(headers: &HeaderMap) -> Result<tokio::time::Instant, Error> {
    let budget = match headers.get("x-timeout-ms") {
        None => DEFAULT_TIMEOUT_MS,
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|budget| *budget > 0)
            .ok_or_else(Error::invalid)?,
    };
    Ok(tokio::time::Instant::now() + Duration::from_millis(budget.min(MAX_TIMEOUT_MS)))
}

/// Apply enabled feature policies; a claim owned by a disabled plugin fails closed.
pub fn authorize_path(
    state: &State,
    headers: &HeaderMap,
    alias: &str,
    path: &str,
    write: bool,
    metadata: bool,
) -> Result<Vec<tokio::sync::OwnedRwLockReadGuard<()>>, Error> {
    state.plugins.authorize_path(
        state,
        &crate::plugin::common::PathAccess {
            headers,
            alias,
            path,
            write,
            metadata,
        },
    )
}
pub use authorize_path as project_path;
