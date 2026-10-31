//! Safe native observations, independent of transcript totals and host price estimates.
use serde_json::{json, Value};
const MAX_SAFE: u64 = 9_007_199_254_740_991;
pub(super) fn context(used: &Value, limit: &Value) -> Option<Value> {
    let limit = limit.as_u64().filter(|n| *n > 0 && *n <= MAX_SAFE)?;
    let used = used.as_u64().filter(|n| *n <= MAX_SAFE);
    Some(json!({"limitTokens":limit,"usedTokens":used}))
}
pub(super) fn empty() -> Value {
    json!({"observedAt":chrono::Utc::now().timestamp_millis()})
}
pub(super) fn codex_context(raw: &Value) -> Option<Value> {
    context(&raw["last"]["totalTokens"], &raw["modelContextWindow"])
}
fn window(id: &str, raw: &Value) -> Option<Value> {
    let used = raw["usedPercent"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0 && *n <= 100.0)?;
    Some(json!({"id":id,"usedPercent":used,
        "windowMinutes":raw["windowDurationMins"].as_u64().filter(|n|*n <= MAX_SAFE),
        "resetsAt":raw["resetsAt"].as_u64().filter(|n|*n <= 8_640_000_000_000).and_then(|n|n.checked_mul(1000))}))
}
pub(super) fn codex_quota(raw: &Value) -> Option<Value> {
    if !raw.is_object() {
        return None;
    }
    let mut windows = Vec::new();
    for key in ["primary", "secondary"] {
        if let Some(value) = window(key, &raw[key]) {
            windows.push(value);
        }
    }
    let balance = raw["credits"]["balance"].as_str().filter(|s| {
        s.len() <= 64
            && !s.is_empty()
            && !s.starts_with('.')
            && !s.ends_with('.')
            && s.matches('.').count() <= 1
            && s.chars().all(|c| c.is_ascii_digit() || c == '.')
    });
    let unlimited = raw["credits"]["unlimited"].as_bool();
    if windows.is_empty() && balance.is_none() && unlimited.is_none() {
        return None;
    }
    Some(json!({"windows":windows,"creditBalance":balance,"unlimited":unlimited}))
}
pub(super) fn codex_quota_response(raw: &Value) -> Option<Value> {
    let legacy = codex_quota(&raw["rateLimits"]);
    let id = raw["rateLimits"]["limitId"].as_str().unwrap_or("codex");
    let bucket = codex_quota(&raw["rateLimitsByLimitId"][id]);
    match (&legacy, &bucket) {
        (Some(old), Some(new))
            if old["windows"].as_array()?.is_empty() && !new["windows"].as_array()?.is_empty() =>
        {
            bucket
        }
        _ => legacy.or(bucket),
    }
}
pub(super) fn claude_quota(raw: &Value) -> Option<Value> {
    let status = raw["status"]
        .as_str()
        .filter(|s| matches!(*s, "allowed" | "allowed_warning" | "rejected"))?;
    let used = raw["utilization"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0 && *n <= 1.0)
        .map(|n| n * 100.0);
    let id = raw["rateLimitType"]
        .as_str()
        .filter(|s| s.len() <= 128 && !s.chars().any(char::is_control))
        .unwrap_or("native");
    Some(
        json!({"windows":[{"id":id,"status":status,"usedPercent":used,
        "resetsAt":raw["resetsAt"].as_u64().filter(|n|*n <= 8_640_000_000_000).and_then(|n|n.checked_mul(1000))}]}),
    )
}
pub(super) fn cost(raw: &Value) -> Option<Value> {
    raw.as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0 && *n <= MAX_SAFE as f64)
        .map(|amount| json!({"amountUsd":amount,"kind":"native-estimate"}))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn latest_explicit_unknown_context_does_not_reuse_an_older_models_capacity() {
        let mut events = super::super::events::Events::new();
        events.push(json!({"method":"thread/tokenUsage/updated","params":{"threadId":"s","tokenUsage":{"last":{"totalTokens":100},"modelContextWindow":200000}}})).unwrap();
        assert_eq!(events.context_usage("s").unwrap()["limitTokens"], 200000);
        events.push(json!({"method":"thread/tokenUsage/updated","params":{"threadId":"s","tokenUsage":{"last":{"totalTokens":100},"modelContextWindow":null}}})).unwrap();
        assert!(events.context_usage("s").unwrap().is_null());
    }
    #[test]
    fn codex_subscription_quota_accepts_multi_bucket_windows_before_credit_only_legacy() {
        let raw = json!({"rateLimits":{"credits":{"balance":"0","unlimited":false}},
            "rateLimitsByLimitId":{"codex":{"primary":{"usedPercent":0,"windowDurationMins":300},"secondary":{"usedPercent":75}}}});
        let quota = codex_quota_response(&raw).unwrap();
        assert_eq!(quota["windows"][0]["usedPercent"].as_f64(), Some(0.0));
        assert_eq!(quota["windows"][1]["usedPercent"].as_f64(), Some(75.0));
        assert!(quota["creditBalance"].is_null());
        assert_eq!(codex_quota_response(&json!({"rateLimits":{"primary":{"usedPercent":25}},"rateLimitsByLimitId":{"codex":{"primary":{"usedPercent":90}}}})).unwrap()["windows"][0]["usedPercent"].as_f64(),Some(25.0));
        assert!(codex_quota_response(
            &json!({"rateLimitsByLimitId":{"unrelated":{"primary":{"usedPercent":90}}}})
        )
        .is_none());
    }
    #[test]
    fn observations_keep_unknowns_and_strip_account_identity() {
        assert_eq!(context(&json!(0), &json!(200000)).unwrap()["usedTokens"], 0);
        assert!(context(&json!(10), &json!(0)).is_none());
        let quota = codex_quota(&json!({"primary":{"usedPercent":25,"resetsAt":1800000000},"email":"private","accessToken":"secret"})).unwrap();
        assert_eq!(quota["windows"][0]["resetsAt"], 1800000000000u64);
        assert!(!quota.to_string().contains("private"));
        assert!(!quota.to_string().contains("secret"));
        assert!(codex_quota(&json!({"primary":{"usedPercent":-1}})).is_none());
        assert_eq!(cost(&json!(0)).unwrap()["amountUsd"], 0.0);
        assert!(cost(&Value::Null).is_none());
    }
}

pub(super) fn report(args: &Value, metrics: &Value, started: std::time::Instant) {
    let errors = metrics["errors"]
        .as_object()
        .map(|values| {
            values
                .iter()
                .map(|(stage, code)| json!({"stage":stage,"code":code}))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    crate::core::events::emit(
        crate::core::events::Level::Info,
        "harness.metrics.read",
        json!({"operation":"harness.metrics","operationId":args["requestId"].as_str().filter(|id|!id.is_empty() && id.len() <= 128 && id.bytes().all(|c|c.is_ascii_alphanumeric() || c == b'-' || c == b'_')),
            "sessionId":args["sessionId"],"stage":"read","result":if errors.is_empty(){"success"}else{"partial"},
            "errors":errors,"contextAvailable":metrics["contextWindow"].is_object(),"quotaAvailable":metrics["quota"].is_object(),"durationMs":started.elapsed().as_millis() as u64}),
    );
}
