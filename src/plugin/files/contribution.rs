use crate::{
    app::State,
    plugin::common::{object, Capabilities},
};
use serde_json::json;
pub(super) fn capabilities(state: &State, identity: usize, enabled: bool) -> Capabilities {
    let client = state.auth.client(identity);
    Capabilities {
        http: object(
            json!({"files":{"read":enabled && !client.exports().is_empty(),"write":enabled && client.can_write(),
        "seq":{"version":if enabled{Some(1)}else{None},"sqlite":enabled,"query":enabled,"nativeTransactions":enabled,"bodies":enabled,"transactionScope":if enabled{Some("file")}else{None}}}}),
        ),
        mcp: object(json!({"fileProtocol":if enabled{Some("fs-agent-http-v1")}else{None}})),
    }
}
