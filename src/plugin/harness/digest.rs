//! Deterministic text projection for authorized analysis, without tools or hidden context.
use crate::core::error::Error;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(super) fn project(history: Value, args: &Value) -> Result<Value, Error> {
    let session = &history["session"];
    let version = json!([session["id"], session["updatedAt"], session["activeTurnId"],
        session["lastTurnId"], session["lastTurnResult"], session["archived"], session["status"]]);
    let revision = format!("{:x}", Sha256::digest(version.to_string().as_bytes()));
    if args["expectedRevision"].as_str().is_some_and(|expected| expected != revision) {
        return Err(Error::conflict("HARNESS_DIGEST_CHANGED"));
    }
    let mut messages = Vec::new();
    let mut omitted = 0usize;
    for turn in history["turns"].as_array().ok_or_else(Error::internal)? {
        for item in turn["items"].as_array().into_iter().flatten() {
            if let Some(message) = message(item, turn, &mut omitted) { messages.push(message); }
        }
    }
    if messages.iter().map(|item| item.to_string().len()).sum::<usize>() > 2 * 1024 * 1024 {
        return Err(Error::too_large("HARNESS_DIGEST_LIMIT"));
    }
    let fields = ["id", "title", "cwd", "status", "statusDetails", "updatedAt", "resumable", "writerLock", "archived", "activeTurnId", "lastTurnId", "lastTurnResult"];
    let mut safe_session = serde_json::Map::new();
    for key in fields { if let Some(value) = session.get(key) { safe_session.insert(key.into(), value.clone()); } }
    Ok(json!({"version":1,"session":safe_session,"revision":revision,"messages":messages,
        "omittedUnclassified":omitted,"nextCursor":history["nextCursor"]}))
}
fn message(item: &Value, turn: &Value, omitted: &mut usize) -> Option<Value> {
    let role = match item["type"].as_str()? {
        "userMessage" => "user",
        "agentMessage" if !matches!(item["phase"].as_str(), Some("analysis" | "commentary")) => "assistant",
        _ => return None,
    };
    let text = if role == "user" { user_text(item, omitted)? } else { super::search::displayed_text(item) };
    if text.trim().is_empty() { return None; }
    Some(json!({"id":item["id"],"turnId":item.get("turnId").unwrap_or(&turn["id"]),
        "role":role,"text":text,"timestamp":item["timestamp"]}))
}
fn user_text(item: &Value, omitted: &mut usize) -> Option<String> {
    let (Some(parts), Some(kinds)) = (item["content"].as_array(), item["contentItemKinds"].as_array()) else {
        *omitted += 1; return None;
    };
    if parts.len() != kinds.len() { *omitted += 1; return None; }
    let mut texts = Vec::new();
    for (part, kind) in parts.iter().zip(kinds) {
        if kind == "user.text" {
            if let Some(text) = part["text"].as_str() { texts.push(text); } else { *omitted += 1; }
        } else if !kind.as_str().is_some_and(|kind| super::search::context_kind(kind)
            || matches!(kind, "user.attachment" | "user.image" | "user.file")) {
            *omitted += 1;
        }
    }
    Some(texts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projects_only_explicit_user_text_and_visible_answers() {
        let history = json!({"session":{"id":"s","updatedAt":1,"status":"idle"},"turns":[{"id":"t","items":[
            {"id":"u","type":"userMessage","content":[{"text":"private rules"},{"text":"build export"}],"contentItemKinds":["agents_md.instructions","user.text"]},
            {"id":"hidden","type":"reasoning","text":"hidden"},{"type":"toolCall","input":"secret"},
            {"id":"comment","type":"agentMessage","phase":"commentary","text":"working"},
            {"id":"answer","type":"agentMessage","phase":"final_answer","text":"done"},
            {"id":"legacy","type":"userMessage","text":"unclassified"}
        ]}],"nextCursor":"older"});
        let result = project(history, &json!({})).unwrap();
        assert_eq!(result["messages"].as_array().unwrap().len(), 2);
        assert_eq!(result["messages"][0]["text"], "build export");
        assert_eq!(result["messages"][1]["text"], "done");
        assert_eq!(result["omittedUnclassified"], 1);
        assert_eq!(result["nextCursor"], "older");
        assert!(!result["messages"].to_string().contains("private rules"));
    }
    #[test]
    fn rejects_changed_snapshot_boundaries() {
        assert_eq!(project(json!({"session":{"id":"s"},"turns":[]}), &json!({"expectedRevision":"old"})).unwrap_err().code, "HARNESS_DIGEST_CHANGED");
    }
}

/// Correlate one read across projection and native RPC diagnostics, without a mutation receipt.
pub(super) fn operation_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    format!("digest-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}
