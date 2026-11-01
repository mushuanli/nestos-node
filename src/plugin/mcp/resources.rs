//! MCP discovery and reading of the caller's logical directory catalog.
use crate::{app::State, core::error::Error};
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn uri(state: &State) -> String {
    format!(
        "cells://{}/directories",
        state.auth.server_id().unwrap_or("")
    )
}

pub(super) fn list(state: &State) -> Value {
    json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","resources":[{
        "uri":uri(state),"name":"accessible_directories","title":"Accessible directories",
        "description":"Current identity's authorized exports, project roots and mounts, effective file/command permissions, and legacy harness workspace identifiers. Logical paths only; no file contents. Read again after authorization or project revisions change.",
        "mimeType":"application/json"}]})
}

pub(super) async fn read(
    state: &Arc<State>,
    identity: usize,
    requested: &str,
    deadline: tokio::time::Instant,
) -> Result<Value, Error> {
    if requested != uri(state) {
        return Err(Error::not_found("RESOURCE_UNKNOWN"));
    }
    let guard = state.files.shared()?;
    let context = state.clone();
    let value = state
        .workers
        .run_ungated(deadline, move |cancel| {
            let _guard = guard;
            context.plugins.directories(&context, identity, &cancel)
        })
        .await?;
    Ok(
        json!({"resultType":"complete","ttlMs":0,"cacheScope":"private",
        "contents":[{"uri":requested,"mimeType":"application/json","text":value.to_string()}]}),
    )
}
