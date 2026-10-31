//! Log lifecycle metadata, never transcript items or native error messages.
use crate::core::events::{emit, Level};
use serde_json::{json, Value};

tokio::task_local! { static OPERATION: Value; }

pub(super) async fn scope<T>(context: Value, future: impl std::future::Future<Output = T>) -> T {
    OPERATION.scope(context, future).await
}

pub(super) fn operation_context(name: &str, args: &Value) -> Value {
    json!({"operation":name,"operationId":label(&args["requestId"]),
        "requestId":label(&args["requestId"]),"profileId":label(&args["profileId"]),
        "projectId":label(&args["projectId"]),"sessionId":label(&args["sessionId"]),
        "epoch":label(&args["epoch"])})
}

pub(super) fn native_context(params: &Value) -> Value {
    let mut context = OPERATION
        .try_with(Clone::clone)
        .unwrap_or_else(|_| json!({}));
    if let Some(id) = label(&params["threadId"]) {
        context["sessionId"] = json!(id);
    }
    context
}

pub(super) fn operation_started(context: &Value) {
    let mut fields = context.clone();
    fields["stage"] = json!("execution");
    fields["result"] = json!("started");
    fields["durationMs"] = json!(0);
    emit(Level::Info, "harness.operation.started", fields);
}

pub(super) fn operation_result(context: &Value, receipt: &Value, elapsed: std::time::Duration) {
    let mut fields = context.clone();
    fields["stage"] = json!("receipt");
    fields["outcome"] = receipt["outcome"].clone();
    fields["result"] = json!(match receipt["outcome"].as_str() {
        Some("committed") => "success",
        Some("not-committed") => "failure",
        _ => "unknown",
    });
    if let Some(id) = label(&receipt["result"]["session"]["id"]) {
        fields["sessionId"] = json!(id);
    }
    fields["code"] = json!(label(&receipt["code"]));
    fields["durationMs"] = json!(elapsed.as_millis() as u64);
    emit(
        if receipt["outcome"] == "committed" {
            Level::Info
        } else {
            Level::Warn
        },
        "harness.operation.completed",
        fields,
    );
}

pub(super) fn record(message: &Value) {
    if let Some((level, name, fields)) = event(message) {
        emit(level, name, fields);
    }
}
fn event(message: &Value) -> Option<(Level, &'static str, Value)> {
    let (level, name) = match message["method"].as_str()? {
        "turn/started" => (Level::Info, "harness.turn.started"),
        "turn/completed" if message["params"]["turn"]["status"] == "failed" => {
            (Level::Warn, "harness.turn.failed")
        }
        "turn/completed" => (Level::Info, "harness.turn.completed"),
        "error" => (Level::Warn, "harness.stream.error"),
        "harness/disconnected" => (Level::Warn, "harness.disconnected"),
        _ => return None,
    };
    Some((level, name, fields(&message["params"])))
}
fn fields(p: &Value) -> Value {
    let error = p
        .get("error")
        .filter(|v| !v.is_null())
        .unwrap_or(&p["turn"]["error"]);
    json!({"sessionId":label(&p["threadId"]),
        "turnId":label(p.get("turnId").unwrap_or(&p["turn"]["id"])),
        "status":label(&p["turn"]["status"]), "willRetry":p["willRetry"].as_bool(),
        "errorInfo":error_info(error), "rpcCode":error["code"].as_i64()})
}
fn label(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| {
        s.len() <= 128
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
    })
}
fn error_info(error: &Value) -> Value {
    let info = &error["codexErrorInfo"];
    if info.is_string() {
        return json!({"kind":label(info)});
    }
    let Some((kind, detail)) = info.as_object().and_then(|o| o.iter().next()) else {
        return Value::Null;
    };
    json!({"kind":label(&json!(kind)), "httpStatusCode":detail["httpStatusCode"].as_u64()})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_calls_keep_operation_identity_across_spawn_without_private_input() {
        let args = json!({"requestId":"request-1","sessionId":"session-1","profileId":"codex",
            "projectId":"project-1","epoch":"epoch-1","prompt":"private-prompt",
            "attachments":[{"content":"private-file"}],"token":"private-token"});
        let task = tokio::spawn(scope(operation_context("harness_resume", &args), async {
            tokio::task::yield_now().await;
            native_context(
                &json!({"threadId":"session-1","cwd":"private-path","input":"private-prompt"}),
            )
        }));
        let fields = task.await.unwrap();
        assert_eq!(fields["operationId"], "request-1");
        assert_eq!(fields["sessionId"], "session-1");
        assert_eq!(fields["profileId"], "codex");
        assert_eq!(fields["projectId"], "project-1");
        assert!(!fields.to_string().contains("private-"));
        assert_eq!(native_context(&json!({})), json!({}));
    }
    #[test]
    fn malformed_identifiers_cannot_inject_log_content() {
        let fields = operation_context(
            "harness_resume",
            &json!({"requestId":"private\nbody",
            "sessionId":"private/path","projectId":"x".repeat(129)}),
        );
        assert!(fields["requestId"].is_null());
        assert!(fields["sessionId"].is_null());
        assert!(fields["projectId"].is_null());
    }
    #[test]
    fn failed_turn_keeps_identifiers_and_transport_category_without_native_details() {
        let message = json!({"method":"turn/completed","params":{"threadId":"session-1","turn":{"id":"turn-1","status":"failed","items":[{"text":"private-message"}],
            "error":{"message":"private-error","additionalDetails":"private-details","codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":403,"body":"private-response"}}}}}});
        let (level, name, fields) = event(&message).unwrap();
        assert_eq!(level, Level::Warn);
        assert_eq!(name, "harness.turn.failed");
        assert_eq!(fields["sessionId"], "session-1");
        assert_eq!(fields["errorInfo"]["kind"], "httpConnectionFailed");
        assert_eq!(fields["errorInfo"]["httpStatusCode"], 403);
        assert!(!fields.to_string().contains("private-"));
    }
    #[test]
    fn normal_reads_and_content_deltas_remain_silent() {
        for method in [
            "item/agentMessage/delta",
            "item/completed",
            "thread/status/changed",
        ] {
            assert!(event(&json!({"method":method})).is_none());
        }
        let (_, name, fields) = event(&json!({"method":"error","params":{"willRetry":true,"error":{"message":"private-error","codexErrorInfo":"responseStreamDisconnected"}}})).unwrap();
        assert_eq!(name, "harness.stream.error");
        assert_eq!(fields["willRetry"], true);
        assert_eq!(fields["errorInfo"]["kind"], "responseStreamDisconnected");
    }
}
