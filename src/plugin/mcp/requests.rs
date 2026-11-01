//! MCP method routing delegates business operations to the appropriate owner.
use super::{protocol::Failure, tools, VERSION};
use crate::{app::State, http::access};
use axum::http::HeaderMap;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) async fn invoke(
    state: &Arc<State>,
    identity: usize,
    headers: &HeaderMap,
    body: &Value,
) -> Result<Value, Failure> {
    match body["method"].as_str().unwrap_or("") {
        "server/discover" => discover(state, identity),
        "ping" => Ok(json!({"resultType":"complete"})),
        "tools/list" => Ok(
            json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":tools::list(state,identity)}),
        ),
        "tools/call" => Ok(tools::call(state, identity, &body["params"]).await),
        "resources/list" => Ok(
            json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","resources":state.plugins.resources(state,identity).map_err(Failure::resource)?}),
        ),
        "resources/templates/list" => Ok(
            json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","resourceTemplates":[]}),
        ),
        "resources/read" => read_resource(state, identity, headers, &body["params"]).await,
        _ => Err(Failure::method()),
    }
}

async fn read_resource(
    state: &Arc<State>,
    identity: usize,
    headers: &HeaderMap,
    params: &Value,
) -> Result<Value, Failure> {
    let deadline = access::deadline(headers).map_err(Failure::resource)?;
    let uri = params["uri"].as_str().unwrap_or("");
    let provider = state
        .plugins
        .resource(state, identity, uri)
        .map_err(Failure::resource)?
        .ok_or_else(|| {
            Failure::resource(crate::core::error::Error::not_found("RESOURCE_UNKNOWN"))
        })?;
    provider
        .read(state, identity, uri, deadline)
        .await
        .map_err(Failure::resource)
}

fn discover(state: &State, identity: usize) -> Result<Value, Failure> {
    let descriptor = tools::descriptor(state, identity).map_err(Failure::resource)?;
    Ok(
        json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","supportedVersions":[VERSION],
        "capabilities":{"tools":{},"resources":{}},
        "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"cells","version":env!("CARGO_PKG_VERSION")},
            "itookit/cells":descriptor}}),
    )
}
