//! Integration fixtures inspect a subscription frame rather than a removed polling tool.
#![allow(dead_code)]
use futures_util::StreamExt;
use cells::{
    app::State,
    auth::{Auth, Client},
    core::error::Error,
    fs::Exports,
    harness::Harnesses,
};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

pub async fn snapshot(service: &Harnesses, args: Value) -> Result<Value, Error> {
    let state = Arc::new(
        State::host(
            Auth::new(
                None,
                vec![Client::new("fixture".into(), None, vec![], vec![])],
            ),
            Exports::new(BTreeMap::new()),
        )
        .unwrap(),
    );
    scoped(service, &state, 0, args).await
}
pub async fn scoped(
    service: &Harnesses,
    state: &Arc<State>,
    identity: usize,
    args: Value,
) -> Result<Value, Error> {
    service
        .subscribe(state, identity, args)
        .await?
        .next()
        .await
        .ok_or_else(Error::unavailable)?
}
