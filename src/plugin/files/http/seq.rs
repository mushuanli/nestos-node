//! Structured SeqFile queries and conditional transactions; arbitrary SQL is never accepted.
use crate::{app::State, core::error::Error, fs::seq, http::access};
use axum::{
    extract::{Path, State as AxumState},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    path: String,
}
pub async fn snapshot(
    AxumState(state): AxumState<Arc<State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Json(query): Json<Query>,
) -> Result<Json<seq::Snapshot>, Error> {
    let (_, export) = access::export(&state, &headers, &alias)?;
    let _project_guard =
        access::authorize_path(&state, &headers, &alias, &query.path, false, false)?;
    let deadline = access::deadline(&headers)?;
    state
        .workers
        .run(&state.files, deadline, move |token| {
            if token.is_cancelled() {
                return Err(Error::cancelled());
            }
            seq::snapshot(&export, &query.path).map(Json)
        })
        .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadQuery {
    path: String,
    selection: seq::Selection,
}
pub async fn query(
    AxumState(state): AxumState<Arc<State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Json(input): Json<ReadQuery>,
) -> Result<Json<seq::Page>, Error> {
    let (_, export) = access::export(&state, &headers, &alias)?;
    let _project_guard =
        access::authorize_path(&state, &headers, &alias, &input.path, false, false)?;
    let deadline = access::deadline(&headers)?;
    state
        .workers
        .run(&state.files, deadline, move |token| {
            if token.is_cancelled() {
                return Err(Error::cancelled());
            }
            seq::query(&export, &input.path, input.selection).map(Json)
        })
        .await
}
pub async fn transaction(
    AxumState(state): AxumState<Arc<State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Json(input): Json<seq::Update>,
) -> Result<(StatusCode, Json<Value>), Error> {
    let (identity, export) = access::writable_export(&state, &headers, &alias)?;
    let _project_guard =
        access::authorize_path(&state, &headers, &alias, &input.path, true, false)?;
    mutate(
        state,
        headers,
        alias,
        identity,
        "transaction",
        move |task, deadline| seq::update(&export, input, || task.checkpoint(deadline)),
    )
    .await
}

pub async fn body(
    AxumState(state): AxumState<Arc<State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Json(input): Json<seq::bodies::BodyQuery>,
) -> Result<Json<Value>, Error> {
    let (_, export) = access::export(&state, &headers, &alias)?;
    let _project_guard =
        access::authorize_path(&state, &headers, &alias, &input.path, false, false)?;
    let _body_guard = access::authorize_path(
        &state,
        &headers,
        &alias,
        &seq::bodies::body_path(&input.path, &input.sha256)?,
        false,
        false,
    )?;
    let deadline = access::deadline(&headers)?;
    state
        .workers
        .run(&state.files, deadline, move |token| {
            if token.is_cancelled() {
                return Err(Error::cancelled());
            }
            seq::bodies::read(&export, &input).map(Json)
        })
        .await
}
pub async fn put_body(
    AxumState(state): AxumState<Arc<State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Json(input): Json<seq::bodies::BodyWrite>,
) -> Result<(StatusCode, Json<Value>), Error> {
    let (identity, export) = access::writable_export(&state, &headers, &alias)?;
    let _project_guard =
        access::authorize_path(&state, &headers, &alias, &input.path, true, false)?;
    let _body_guard = access::authorize_path(
        &state,
        &headers,
        &alias,
        &seq::bodies::body_path(&input.path, &input.sha256)?,
        true,
        false,
    )?;
    mutate(
        state,
        headers,
        alias,
        identity,
        "body.publish",
        move |task, deadline| seq::bodies::write(&export, input, || task.checkpoint(deadline)),
    )
    .await
}

async fn mutate(
    state: Arc<State>,
    headers: HeaderMap,
    alias: String,
    identity: usize,
    stage: &'static str,
    job: impl FnOnce(Arc<crate::operations::Operation>, tokio::time::Instant) -> Result<Value, Error>
        + Send
        + 'static,
) -> Result<(StatusCode, Json<Value>), Error> {
    let started = std::time::Instant::now();
    let deadline = access::deadline(&headers)?;
    let gate = state.files.shared()?;
    let operation_id = access::operation_id(&headers)?.to_owned();
    let operation = state
        .services
        .require::<crate::operations::Operations>()?
        .register(identity, &alias, &operation_id)?;
    let task = operation.clone();
    let completion = operation.clone();
    tokio::spawn(async move {
        let result = match state.workers.acquire(deadline).await {
            Ok(permit) => tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let _gate = gate;
                job(task, deadline)
            })
            .await
            .map_err(|_| Error::internal())
            .and_then(|result| result),
            Err(error) => Err(error),
        };
        report(&operation_id, stage, started, &result);
        completion.finish(result);
    });
    // The detached task owns admission; losing the HTTP response never triggers replay.
    let (status, receipt) = operation.wait(deadline).await;
    Ok((status, Json(receipt)))
}
fn report(
    operation_id: &str,
    stage: &str,
    started: std::time::Instant,
    result: &Result<Value, Error>,
) {
    use crate::core::events::{emit, Level};
    let error = result.as_ref().err();
    emit(
        if error.is_some() {
            Level::Warn
        } else {
            Level::Info
        },
        "seq.operation",
        serde_json::json!({
            "operationId": operation_id, "stage": stage, "result": if error.is_some() {"failure"} else {"success"},
            "code": error.map(|e| e.code), "outcome": error.map(|e| e.outcome.unwrap_or("not-committed")).unwrap_or("committed"),
            "durationMs": started.elapsed().as_millis()
        }),
    );
}
