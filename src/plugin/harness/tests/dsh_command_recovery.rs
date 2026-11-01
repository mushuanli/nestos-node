//! Native responses, rather than matching command history text, confirm serial commands.
use super::{fixture, mutate, service_options};
use cells::harness::Harnesses;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

fn count(host: &fixture::Host, endpoint: &str) -> usize {
    host.state
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|row| row["method"] == endpoint)
        .count()
}
async fn inspect(service: &Harnesses, operation: &str) -> Value {
    service
        .call(
            "harness_operation",
            json!({"profileId":"dsh","epoch":"epoch","requestId":operation}),
        )
        .await
        .unwrap()
}
async fn confirmed(service: &Harnesses, operation: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let receipt = inspect(service, operation).await;
            if receipt["outcome"] == "committed" {
                return receipt;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("original command must finish without being reissued")
}

#[tokio::test]
async fn long_command_outlives_the_short_query_deadline_without_duplicate_native_execution() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service_options(root.path(), &host.url, "request_timeout_ms=100");
    mutate(
        &service,
        "harness_resume",
        "long-resume",
        json!({"sessionId":"existing"}),
    )
    .await;
    host.state
        .delays
        .lock()
        .unwrap()
        .insert("commands/execute".into(), 450);
    let args = json!({"sessionId":"existing","command":"compact"});
    let result = mutate(&service, "harness_command", "long-command", args.clone()).await;
    assert_eq!(result["outcome"], "committed");
    assert_eq!(result["result"]["commandId"], "native-command");
    assert_eq!(inspect(&service, "long-command").await, result);
    assert_eq!(
        mutate(&service, "harness_command", "long-command", args).await,
        result
    );
    assert_eq!(count(&host, "commands/execute"), 1);
    service.close().await;
}

#[tokio::test]
async fn original_command_response_is_persisted_after_the_callers_request_is_dropped() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = Arc::new(service_options(
        root.path(),
        &host.url,
        "request_timeout_ms=100",
    ));
    mutate(
        &service,
        "harness_resume",
        "detached-resume",
        json!({"sessionId":"existing"}),
    )
    .await;
    host.state
        .delays
        .lock()
        .unwrap()
        .insert("commands/execute".into(), 450);
    let running = service.clone();
    let request = tokio::spawn(async move {
        mutate(
            &running,
            "harness_command",
            "detached-command",
            json!({"sessionId":"existing","command":"compact"}),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while count(&host, "commands/execute") == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    request.abort();
    let _ = request.await;
    assert_eq!(
        confirmed(&service, "detached-command").await["result"]["commandId"],
        "native-command"
    );
    assert_eq!(count(&host, "commands/execute"), 1);
    service.close().await;
}

#[tokio::test]
async fn command_deadline_remains_bounded_and_a_true_unknown_still_blocks_new_mutations() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service_options(
        root.path(),
        &host.url,
        "request_timeout_ms=100\ncommand_timeout_ms=100",
    );
    mutate(
        &service,
        "harness_resume",
        "bounded-resume",
        json!({"sessionId":"existing"}),
    )
    .await;
    host.state
        .delays
        .lock()
        .unwrap()
        .insert("commands/execute".into(), 450);
    let args = json!({"sessionId":"existing","command":"compact"});
    let unknown = mutate(&service, "harness_command", "bounded-command", args.clone()).await;
    assert_eq!(unknown["outcome"], "unknown");
    assert_eq!(unknown["code"], "ETIMEDOUT");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(inspect(&service, "bounded-command").await, unknown);
    assert_eq!(
        mutate(&service, "harness_command", "bounded-command", args.clone()).await,
        unknown
    );
    let blocked = mutate(&service, "harness_command", "blocked-command", args).await;
    assert_eq!(blocked["outcome"], "not-committed");
    assert_eq!(blocked["code"], "DSH_COMMAND_RESULT_PENDING");
    let query = service.call("harness_query", json!({"profileId":"dsh","sessionId":"existing","command":"status","requestId":"bounded-status"})).await.unwrap();
    assert_eq!(query["status"], "completed");
    assert_eq!(count(&host, "commands/execute"), 1);
    service.close().await;
}

#[tokio::test]
async fn query_rpc_keeps_its_short_deadline_while_command_rpc_uses_its_own_budget() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service_options(
        root.path(),
        &host.url,
        "request_timeout_ms=100\ncommand_timeout_ms=2000",
    );
    host.state
        .delays
        .lock()
        .unwrap()
        .insert("session/modelCatalog".into(), 450);
    let error = service
        .call(
            "harness_models",
            json!({"profileId":"dsh","sessionId":"existing"}),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "ETIMEDOUT");
    assert_eq!(count(&host, "commands/execute"), 0);
    service.close().await;
}

#[tokio::test]
async fn service_response_deadline_does_not_cancel_native_command_or_let_the_next_command_bypass_it(
) {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = Arc::new(service_options(
        root.path(),
        &host.url,
        "request_timeout_ms=100\ncommand_timeout_ms=30000",
    ));
    mutate(
        &service,
        "harness_resume",
        "service-deadline-resume",
        json!({"sessionId":"existing"}),
    )
    .await;
    host.state
        .delays
        .lock()
        .unwrap()
        .insert("commands/execute".into(), 26_000);
    let args = json!({"sessionId":"existing","command":"compact"});
    let started = std::time::Instant::now();
    let initial = mutate(
        &service,
        "harness_command",
        "service-deadline-command",
        args.clone(),
    )
    .await;
    assert_eq!(initial["outcome"], "unknown");
    assert!(started.elapsed() >= Duration::from_secs(25));
    assert_eq!(
        mutate(
            &service,
            "harness_command",
            "service-deadline-command",
            args.clone()
        )
        .await,
        initial
    );
    host.state.delays.lock().unwrap().remove("commands/execute");
    let next_service = service.clone();
    let next_args = args.clone();
    let mut next = tokio::spawn(async move {
        mutate(
            &next_service,
            "harness_command",
            "service-deadline-next",
            next_args,
        )
        .await
    });
    assert!(tokio::time::timeout(Duration::from_millis(50), &mut next)
        .await
        .is_err());
    assert_eq!(count(&host, "commands/execute"), 1);
    let query = service.call("harness_query", json!({"profileId":"dsh","sessionId":"existing","command":"status","requestId":"live-status"})).await.unwrap();
    assert_eq!(query["status"], "completed");
    let receipt = confirmed(&service, "service-deadline-command").await;
    assert_eq!(receipt["result"]["commandId"], "native-command");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), next)
            .await
            .unwrap()
            .unwrap()["outcome"],
        "committed"
    );
    assert_eq!(count(&host, "commands/execute"), 2);
    service.close().await;
}
