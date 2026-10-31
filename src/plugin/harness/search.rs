//! Searches parsed, authorized native history instead of scanning an entire harness home.
use super::{bridge::Bridge, codex::Codex};
use crate::core::{error::Error, search::KeywordQuery};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::{collections::HashSet, time::Duration};
static SEARCHES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

pub async fn search(codex: &Codex, bridge: &Bridge, args: &Value) -> Result<Value, Error> {
    search_with(
        args,
        |input| Box::pin(async move { codex.sessions(bridge, &input).await }),
        |input| Box::pin(async move { codex.history(bridge, &input).await }),
    )
    .await
}

pub(super) async fn search_with<'a>(
    args: &Value,
    list: impl FnMut(Value) -> BoxFuture<'a, Result<Value, Error>>,
    history: impl FnMut(Value) -> BoxFuture<'a, Result<Value, Error>>,
) -> Result<Value, Error> {
    let _permit = SEARCHES
        .try_acquire()
        .map_err(|_| Error::too_many("SEARCH_BUSY"))?;
    let query = args["query"]
        .as_str()
        .filter(|q| !q.trim().is_empty() && q.len() <= 1024 && !q.contains('\0'))
        .ok_or_else(Error::invalid)?;
    let mode = args["mode"]
        .as_str()
        .filter(|m| matches!(*m, "title" | "content" | "prompt"))
        .ok_or_else(Error::invalid)?;
    tokio::time::timeout(
        Duration::from_secs(15),
        collect(args, &KeywordQuery::new(query), mode, list, history),
    )
    .await
    .map_err(|_| Error::timed_out())?
}
async fn collect<'a>(
    args: &Value,
    query: &KeywordQuery,
    mode: &str,
    mut list: impl FnMut(Value) -> BoxFuture<'a, Result<Value, Error>>,
    mut history: impl FnMut(Value) -> BoxFuture<'a, Result<Value, Error>>,
) -> Result<Value, Error> {
    let mut listing = args.clone();
    listing["limit"] = json!(100);
    listing["cursor"] = Value::Null;
    let mut matches = Vec::new();
    let mut truncated = false;
    let mut bytes = 0usize;
    let mut seen = HashSet::new();
    for _ in 0..20 {
        let page = list(listing.clone()).await?;
        for session in page["sessions"].as_array().ok_or_else(Error::internal)? {
            if mode == "title" {
                if session["title"]
                    .as_str()
                    .is_some_and(|title| query.matches(title))
                {
                    matches.push(json!({"sessionId":session["id"],"title":session["title"],"summary":session["title"],"updatedAt":session["updatedAt"]}));
                }
            } else {
                search_history(
                    &mut history,
                    args,
                    session,
                    query,
                    mode,
                    &mut matches,
                    &mut bytes,
                    &mut truncated,
                )
                .await?;
            }
            if matches.len() >= 100 || bytes >= 16 * 1024 * 1024 {
                truncated = true;
                break;
            }
        }
        if truncated || page["nextCursor"].is_null() {
            listing["cursor"] = Value::Null;
            break;
        }
        let cursor = page["nextCursor"].as_str().ok_or_else(Error::internal)?;
        if !seen.insert(cursor.to_string()) {
            return Err(Error::internal());
        }
        listing["cursor"] = json!(cursor);
    }
    truncated |= !listing["cursor"].is_null();
    matches.truncate(100);
    Ok(json!({"matches":matches,"truncated":truncated,"nextCursor":null}))
}
async fn search_history<'a>(
    history: &mut impl FnMut(Value) -> BoxFuture<'a, Result<Value, Error>>,
    args: &Value,
    session: &Value,
    query: &KeywordQuery,
    mode: &str,
    matches: &mut Vec<Value>,
    bytes: &mut usize,
    truncated: &mut bool,
) -> Result<(), Error> {
    let mut input = args.clone();
    input["sessionId"] = session["id"].clone();
    input["toolDetail"] = json!("summary");
    input["cursor"] = Value::Null;
    let mut seen = HashSet::new();
    for _ in 0..64 {
        let page = history(input.clone()).await?;
        *bytes += page.to_string().len();
        if *bytes > 16 * 1024 * 1024 {
            *truncated = true;
            break;
        }
        for turn in page["turns"].as_array().ok_or_else(Error::internal)? {
            for item in turn["items"].as_array().into_iter().flatten() {
                let text = search_text(item, mode == "prompt");
                if !query.matches(&text) {
                    continue;
                }
                matches.push(json!({"sessionId":session["id"],"title":session["title"],"updatedAt":session["updatedAt"],
                    "turnId":if item["turnId"].is_string() {&item["turnId"]} else {&turn["id"]},"itemId":item["id"],"summary":excerpt(&text, query.anchor())}));
                if matches.len() >= 100 {
                    *truncated = true;
                    return Ok(());
                }
            }
        }
        if page["nextCursor"].is_null() {
            return Ok(());
        }
        let cursor = page["nextCursor"].as_str().ok_or_else(Error::internal)?;
        if !seen.insert(cursor.to_string()) {
            return Err(Error::internal());
        }
        input["cursor"] = json!(cursor);
    }
    *truncated = true;
    Ok(())
}
pub(super) fn displayed_text(item: &Value) -> String {
    match item["type"].as_str() {
        Some("agentMessage" | "userMessage") => item["text"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| text_parts(&item["content"]).join("\n")),
        Some("commandExecution") => item["commandPreview"]
            .as_str()
            .or(item["command"].as_str())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .into(),
        _ => String::new(),
    }
}
pub(super) fn search_text(item: &Value, prompt: bool) -> String {
    if !prompt {
        return displayed_text(item);
    }
    if item["type"] != "userMessage" {
        return String::new();
    }
    if let (Some(parts), Some(kinds)) = (
        item["content"].as_array(),
        item["contentItemKinds"].as_array(),
    ) {
        if parts.len() == kinds.len() {
            return parts
                .iter()
                .zip(kinds)
                .filter(|(_, kind)| !context_kind(kind.as_str().unwrap_or("")))
                .flat_map(|(part, _)| text_parts(part))
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    displayed_text(item)
}
pub(super) fn context_kind(kind: &str) -> bool {
    matches!(
        kind,
        "agents_md.instructions"
            | "environments.environment_context"
            | "goal.internal_context"
            | "permissions.approved_command_prefix_saved"
            | "permissions.instructions"
            | "collaboration_mode.instructions"
            | "host_skills.instructions"
            | "multi_agent.mode_instructions"
            | "multi_agent.role_instructions"
            | "model_switch.instructions"
            | "daemon_recovery.internal_context"
            | "plugins.recommendations"
            | "apps.instructions"
            | "plugins.usage_instructions"
            | "multi_agent.usage_hint"
    )
}
fn text_parts(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(parts) => parts.iter().flat_map(text_parts).collect(),
        Value::Object(_) => text_parts(if value["text"].is_string() {
            &value["text"]
        } else {
            &value["content"]
        }),
        _ => vec![],
    }
}
fn excerpt(text: &str, query: &str) -> String {
    let start = text.to_lowercase().find(&query.to_lowercase()).unwrap_or(0);
    // Lowercasing can change UTF-8 length; character iteration keeps excerpts valid.
    let start = text
        .char_indices()
        .take_while(|(at, _)| *at < start)
        .count()
        .saturating_sub(80);
    text.chars().skip(start).take(500).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompt_scope_filters_roles_and_explicit_context_but_keeps_unknown_origins() {
        assert_eq!(
            search_text(
                &json!({"type":"agentMessage","text":"assistant only"}),
                true
            ),
            ""
        );
        let user = json!({"type":"userMessage","content":[{"text":"instructions"},{"text":"question"},{"text":"unknown provenance"}],
            "contentItemKinds":["agents_md.instructions","user","future.kind"]});
        assert_eq!(search_text(&user, true), "question\nunknown provenance");
        assert_eq!(
            search_text(&user, false),
            "instructions\nquestion\nunknown provenance"
        );
    }
    #[test]
    fn matches_displayed_array_text_and_escaped_json_without_tool_payloads() {
        let item = json!({"type":"userMessage","content":[{"type":"text","text":"line\nquoted \"text\""},{"text":"中文"}]});
        assert_eq!(displayed_text(&item), "line\nquoted \"text\"\n中文");
        assert_eq!(
            displayed_text(&json!({"type":"toolOutput","text":"secret"})),
            ""
        );
        assert!(!excerpt("中文 quoted", "quoted").is_empty());
    }
}
