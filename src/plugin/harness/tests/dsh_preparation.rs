//! Failed observations cannot create an unknown native mutation or consume its queue slot.
use super::{fixture, mutate, service};
use cells::harness::Harnesses;
use serde_json::{json, Value};

fn count(host: &fixture::Host, endpoint: &str) -> usize {
    host.state
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|row| row["method"] == endpoint)
        .count()
}

fn fail_after(host: &fixture::Host, endpoint: &str, skip: usize) {
    host.state
        .errors_after
        .lock()
        .unwrap()
        .insert(endpoint.into(), skip);
    host.state.errors.lock().unwrap().insert(
        endpoint.into(),
        json!({"code":"gateway/internal","message":"private-discovery-detail"}),
    );
}

fn assert_not_started(receipt: &Value) {
    assert_eq!(receipt["outcome"], "not-committed");
    assert_eq!(receipt["code"], "EIO");
    assert_eq!(receipt["stage"], "prepare");
    assert_eq!(receipt["executionStarted"], false);
    assert!(!receipt.to_string().contains("private-discovery-detail"));
}

async fn inspect(service: &Harnesses, id: &str) -> Value {
    service
        .call(
            "harness_operation",
            json!({"profileId":"dsh","epoch":"epoch","requestId":id}),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn failed_command_discovery_is_not_an_unknown_native_execution() {
    // Both the service discovery and the adapter's fresh pre-dispatch check must be covered.
    for skip in [0, 1] {
        check_failed_command_discovery(skip).await;
    }
}

async fn check_failed_command_discovery(skip: usize) {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let created = mutate(
        &service,
        "harness_create",
        "preflight-create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].as_str().unwrap();
    fail_after(&host, "commands/list", skip);
    let args = json!({"sessionId":id,"command":"compact"});
    let rejected = mutate(
        &service,
        "harness_command",
        "preflight-failed",
        args.clone(),
    )
    .await;
    assert_not_started(&rejected);
    assert_eq!(count(&host, "commands/execute"), 0);
    host.state.errors.lock().unwrap().remove("commands/list");
    assert_eq!(
        mutate(
            &service,
            "harness_command",
            "preflight-failed",
            args.clone()
        )
        .await,
        rejected
    );
    assert_eq!(inspect(&service, "preflight-failed").await, rejected);
    assert_eq!(
        mutate(&service, "harness_command", "preflight-new-attempt", args).await["outcome"],
        "committed"
    );
    assert_eq!(count(&host, "commands/execute"), 1);
    service.close().await;
}

fn model_call(id: &str) -> Value {
    json!({"sessionId":id,"prompt":"model preparation fixture", "selection":{"modelId":"[\"fixture\",\"model\"]","reasoningEffort":"high"}})
}

#[tokio::test]
async fn failed_model_catalog_does_not_claim_a_prompt_or_native_writer() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    mutate(
        &service,
        "harness_resume",
        "preflight-resume",
        json!({"sessionId":"existing"}),
    )
    .await;
    fail_after(&host, "session/modelCatalog", 0);
    let args = model_call("existing");
    let rejected = mutate(&service, "harness_turn", "failed-model-read", args.clone()).await;
    assert_not_started(&rejected);
    assert_eq!(count(&host, "session/selectModel"), 0);
    assert_eq!(count(&host, "session/prompt"), 0);
    host.state
        .errors
        .lock()
        .unwrap()
        .remove("session/modelCatalog");
    assert_eq!(inspect(&service, "failed-model-read").await, rejected);
    assert_eq!(
        mutate(&service, "harness_turn", "failed-model-read", args.clone()).await,
        rejected
    );
    assert_eq!(
        mutate(&service, "harness_turn", "retry-model-read", args).await["outcome"],
        "committed"
    );
    assert_eq!(count(&host, "session/selectModel"), 1);
    assert_eq!(count(&host, "session/prompt"), 1);
    service.close().await;
}

#[tokio::test]
async fn unknown_model_selection_still_blocks_a_prompt_that_was_never_sent() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    mutate(
        &service,
        "harness_resume",
        "selection-resume",
        json!({"sessionId":"existing"}),
    )
    .await;
    fail_after(&host, "session/selectModel", 0);
    let args = model_call("existing");
    let unknown = mutate(&service, "harness_turn", "unknown-selection", args.clone()).await;
    assert_eq!(unknown["outcome"], "unknown");
    assert!(unknown.get("executionStarted").is_none());
    assert_eq!(inspect(&service, "unknown-selection").await, unknown);
    assert_eq!(
        mutate(&service, "harness_turn", "unknown-selection", args.clone()).await,
        unknown
    );
    let blocked = mutate(&service, "harness_turn", "after-unknown-selection", args).await;
    assert_eq!(blocked["code"], "DSH_SUBMISSION_RESULT_PENDING");
    assert_eq!(count(&host, "session/selectModel"), 1);
    assert_eq!(count(&host, "session/prompt"), 0);
    service.close().await;
}
