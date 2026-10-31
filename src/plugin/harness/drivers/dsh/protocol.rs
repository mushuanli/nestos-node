//! Pure DSH durable-record and assistant-stream projection into harness envelopes.
use serde_json::{json, Value};

#[cfg(test)]
mod command_tests {
    use super::*;
    #[test]
    fn command_history_keeps_native_identity_and_result_separate_from_model_messages() {
        let items = history_records(
            &[
                json!({"event":{"seq":1,"type":"command/run","data":{"commandId":"c","name":"compact"}}}),
                json!({"event":{"seq":2,"type":"command/done","data":{"commandId":"c","kind":"success","text":"Compacted"}}}),
            ],
            "s",
        );
        assert_eq!(items[0]["id"], "command:c");
        assert_eq!(items[1]["id"], "command:c");
        assert_eq!(items[1]["text"], "Compacted");
        assert_eq!(items[1]["type"], "systemMessage");
    }
}

#[derive(Default)]
pub struct Cursor {
    pub seq: Option<u64>,
    pub turn: String,
    pub attempt: String,
    pub native_turn: Option<u64>,
    pub(super) observed_native: Option<u64>,
    pub(super) expected_request: Option<String>,
    pub(super) control_pending: Option<String>,
    pub(super) admission: Option<super::admission::PromptAdmission>,
    pub(super) command_pending: Option<super::command_admission::CommandAdmission>,
    pub(super) tools: std::collections::BTreeMap<String, Value>,
    pub(super) commands: std::collections::BTreeMap<String, String>,
}
impl Cursor {
    pub(super) fn busy(&self) -> bool {
        !self.turn.is_empty()
            || self.admission.is_some()
            || self.control_pending.is_some()
            || self.command_pending.is_some()
            || !self.commands.is_empty()
    }
}
pub fn session(row: &Value, owned: bool, archived: bool) -> Value {
    let title = &row["projections"]["values"]["title"];
    json!({"id":row["sessionId"],"title":title.as_str().or(title["title"].as_str()).unwrap_or(""),
        "cwd":row["cwd"],"status":if row["running"]==true{"active"}else{"idle"},
        "createdAt":row["createdAt"],"updatedAt":row["updatedAt"],"parentSessionId":row["parentSessionId"],
        "owned":owned,"resumable":!archived,"forkable":false,"archived":archived,
        "modelSelection":row["modelSelection"],
        "archiveAllowed":!archived && row["running"] != true})
}
pub fn history_records(records: &[Value], session: &str) -> Vec<Value> {
    let mut cursor = Cursor::default();
    records
        .iter()
        .flat_map(|record| {
            // Durable messages carry native turn numbers, including pages starting mid-turn.
            if let Some(turn) = record["event"]["data"]["turn"].as_u64() {
                cursor.turn = format!("{session}-{turn}");
            }
            record_items(&record["event"], &mut cursor, session)
        })
        .collect()
}
pub fn records(records: &[Value], cursor: &mut Cursor, session: &str) -> Vec<Value> {
    records
        .iter()
        .flat_map(|record| record_items(&record["event"], cursor, session))
        .collect()
}
fn record_items(event: &Value, cursor: &mut Cursor, session: &str) -> Vec<Value> {
    let Some(seq) = event["seq"].as_u64() else {
        return vec![];
    };
    if cursor.seq.is_some_and(|prior| seq <= prior) {
        return vec![];
    }
    cursor.seq = Some(seq);
    if let Some(admission) = cursor.admission.as_mut() {
        admission.observe(event, session);
    }
    if cursor.native_turn.is_none()
        && surface_appends(&event["surfaceOp"])
        && event["type"] == "user/message"
        && event["data"]["source"]["kind"] == "user"
        && cursor
            .expected_request
            .as_deref()
            .is_some_and(|id| event["data"]["source"]["rpcId"] == id)
        && !cursor
            .admission
            .as_ref()
            .is_some_and(|value| value.mismatch)
    {
        cursor.native_turn = event["data"]["turn"].as_u64().or(cursor.observed_native);
        cursor.observed_native = cursor.native_turn;
    }
    if matches!(event["type"].as_str(), Some("command/run" | "command/done")) {
        let data = &event["data"];
        let Some(id) = data["commandId"].as_str() else {
            return vec![];
        };
        let running = event["type"] == "command/run";
        if running {
            cursor.commands.insert(
                id.into(),
                format!("/{}", data["name"].as_str().unwrap_or("")),
            );
        }
        let name = if running {
            cursor.commands.get(id).cloned()
        } else {
            cursor.commands.remove(id)
        };
        return vec![json!({"id":format!("command:{id}"),"type":"systemMessage",
            "text":data["text"].as_str().map(str::to_owned).or(name).unwrap_or_else(||"Command".into()),
            "status":if running {"running"} else if data["kind"]=="error" {"failed"} else {"completed"}})];
    }
    if event["type"] == "turn/start" {
        cursor.observed_native = event["data"]["turn"].as_u64();
        if cursor.expected_request.is_none() {
            cursor.native_turn = cursor.observed_native;
        }
        if cursor.turn.is_empty() {
            cursor.turn = format!("{session}-{}", event["data"]["turn"]);
        }
        return vec![];
    }
    let role = match event["type"].as_str() {
        Some("user/message") => "userMessage",
        Some("assistant/message") => "agentMessage",
        Some("tool/result") => "toolResult",
        _ => return vec![],
    };
    if !surface_appends(&event["surfaceOp"]) {
        return vec![];
    }
    let mut items = content_items(event, cursor, role, seq);
    let turn = visible_turn(cursor, session);
    for item in &mut items {
        item["turnId"] = json!(turn);
    }
    items
}
fn visible_turn(cursor: &Cursor, session: &str) -> String {
    if cursor.expected_request.is_some()
        && (cursor.native_turn.is_none() || cursor.native_turn != cursor.observed_native)
    {
        return cursor
            .observed_native
            .map(|turn| format!("{session}-{turn}"))
            .unwrap_or_default();
    }
    cursor.turn.clone()
}
fn content_items(event: &Value, cursor: &mut Cursor, role: &str, seq: u64) -> Vec<Value> {
    let data = &event["data"];
    let content = data["content"]
        .as_array()
        .or(data["message"]["content"].as_array());
    let Some(content) = content else {
        return vec![];
    };
    content
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            let id = if role == "agentMessage" && !cursor.attempt.is_empty() {
                format!("{}-{index}", cursor.attempt)
            } else {
                format!("dsh-{seq}-{index}")
            };
            let mut value = item(block, &id, role, &cursor.turn, &event["time"])?;
            if role == "userMessage" && block["type"] == "text" && value["text"].is_string() {
                value["content"] = json!([{"type":"text","text":value["text"]}]);
                value["contentItemKinds"] = json!(["user.text"]);
            }
            tool_result(&mut value, block, cursor);
            Some(value)
        })
        .collect()
}
fn tool_result(value: &mut Value, block: &Value, cursor: &mut Cursor) {
    if block["type"] == "tool-call" {
        value["status"] = json!("inProgress");
        if let Some(id) = value["id"].as_str() {
            let summary =
                crate::harness::presentation::item(value).unwrap_or_else(|| value.clone());
            cursor.tools.insert(id.into(), summary);
            while cursor.tools.len() > 256 {
                cursor.tools.pop_first();
            }
        }
    } else if block["type"] == "tool-result" {
        if let Some(prior) = value["id"].as_str().and_then(|id| cursor.tools.remove(id)) {
            for key in ["tool", "name", "paths", "operation", "commandPreview"] {
                if !prior[key].is_null() {
                    value[key] = prior[key].clone();
                }
            }
        }
    }
}
pub(super) fn surface_appends(op: &Value) -> bool {
    op.is_null() || op == "append" || op["op"] == "append"
}
fn item(block: &Value, id: &str, role: &str, turn: &str, time: &Value) -> Option<Value> {
    let mut value = match block["type"].as_str()? {
        "text" if role == "userMessage" => {
            json!({"id":id,"type":role,"content":[{"type":"inputText","text":block["text"]}]})
        }
        "text" => json!({"id":id,"type":role,"text":block["text"]}),
        "reasoning" => json!({"id":id,"type":"reasoning","text":block["text"]}),
        "tool-call" => {
            json!({"id":block["id"],"type":"dynamicToolCall","tool":block["name"],"arguments":block["arguments"]})
        }
        "tool-result" => {
            json!({"id":block["toolCallId"],"type":"dynamicToolCall","status":if block["isError"]==true{"failed"}else{"completed"}})
        }
        _ => return None,
    };
    value["turnId"] = if turn.is_empty() {
        Value::Null
    } else {
        json!(turn)
    };
    value["timestamp"] = if time.is_number() {
        time.clone()
    } else {
        Value::Null
    };
    if value["status"].is_null() {
        value["status"] = json!("completed");
    }
    Some(value)
}
pub fn frame(frame: &Value, cursor: &mut Cursor, session: &str) -> Vec<Value> {
    match frame["type"].as_str() {
        Some("event") => event(frame, cursor, session),
        Some("assistant-stream") => assistant(&frame["frame"], cursor, session),
        _ => vec![],
    }
}
fn event(frame: &Value, cursor: &mut Cursor, session: &str) -> Vec<Value> {
    let event = &frame["event"];
    if event["seq"]
        .as_u64()
        .is_some_and(|seq| cursor.seq.is_some_and(|prior| seq <= prior))
    {
        return vec![];
    }
    let was_unbound = cursor.native_turn.is_none();
    let mut messages = records(std::slice::from_ref(frame), cursor, session).into_iter()
        .map(|item| json!({"method":"item/completed","params":{"threadId":session,"turnId":item["turnId"],"item":item}})).collect::<Vec<_>>();
    if was_unbound && cursor.native_turn.is_some() {
        messages.push(json!({"method":"turn/started","params":{"threadId":session,"turn":{"id":cursor.turn,"status":"inProgress"}}}));
    }
    if event["type"] == "turn/end"
        && !cursor.turn.is_empty()
        && cursor.native_turn == event["data"]["turn"].as_u64()
        && cursor.native_turn.is_some()
    {
        let result = terminal_result(&event["data"]["reason"]);
        messages.push(json!({"method":"turn/completed","params":{"threadId":session,"turn":{"id":cursor.turn,"status":result}}}));
        cursor.turn.clear();
        cursor.native_turn = None;
        cursor.expected_request = None;
    }
    if event["type"] == "turn/end" && cursor.observed_native == event["data"]["turn"].as_u64() {
        cursor.observed_native = None;
    }
    messages
}
fn assistant(frame: &Value, cursor: &mut Cursor, session: &str) -> Vec<Value> {
    let turn = visible_turn(cursor, session);
    if frame["type"] == "start" {
        cursor.attempt = frame["attemptId"].as_str().unwrap_or("").into();
    }
    if frame["type"] != "chunk" {
        return vec![];
    }
    let chunk = &frame["chunk"];
    let id = format!(
        "{}-{}",
        cursor.attempt,
        chunk["index"].as_u64().unwrap_or(0)
    );
    match chunk["type"].as_str() {
        Some("text-delta") => vec![json!({"method":"item/agentMessage/delta","params":{
            "threadId":session,"turnId":turn,"itemId":id,"delta":chunk["text"]}})],
        Some("reasoning-delta") => vec![json!({"method":"item/reasoning/textDelta","params":{
            "threadId":session,"turnId":turn,"itemId":id,"delta":chunk["text"]}})],
        _ => vec![],
    }
}
pub fn baseline(snapshot: &Value, cursor: &mut Cursor, session: &str) -> Vec<Value> {
    let attempt = &snapshot["assistantStream"]["activeAttempt"];
    let Some(id) = attempt["attemptId"].as_str() else {
        return vec![];
    };
    cursor.attempt = id.into();
    attempt["stream"].as_array().into_iter().flatten().flat_map(|row| {
        let chunk=match row["type"].as_str() {
            Some("chunk")=>row["chunk"].clone(),
            Some("text-chunks"|"reasoning-chunks")=>json!({"index":row["index"],
                "type":if row["type"]=="text-chunks"{"text-delta"}else{"reasoning-delta"},
                "text":row["texts"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<String>()}),
            _=>return vec![],
        };
        assistant(&json!({"type":"chunk","chunk":chunk}),cursor,session)
    }).collect()
}
pub fn turns(items: Vec<Value>) -> Vec<Value> {
    let mut turns: Vec<Value> = Vec::new();
    for item in items {
        if turns.last().is_none_or(|turn| turn["id"] != item["turnId"]) {
            turns.push(json!({"id":item["turnId"],"items":[]}));
        }
        turns.last_mut().unwrap()["items"]
            .as_array_mut()
            .unwrap()
            .push(item);
    }
    turns
}

pub(super) fn terminal_result(reason: &Value) -> &'static str {
    match reason["kind"].as_str() {
        Some("completed") => "completed",
        Some("cancelled" | "interrupted" | "aborted") => "interrupted",
        _ => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attaching_to_a_running_attempt_includes_its_already_streamed_text() {
        let mut cursor = Cursor {
            turn: "active".into(),
            ..Default::default()
        };
        let snapshot = json!({"assistantStream":{"activeAttempt":{"attemptId":"attempt","stream":[
            {"type":"text-chunks","index":0,"texts":["hello ","world"]}]}}});
        let messages = baseline(&snapshot, &mut cursor, "s");
        assert_eq!(messages[0]["params"]["delta"], "hello world");
        assert_eq!(messages[0]["params"]["itemId"], "attempt-0");
    }
    #[test]
    fn durable_messages_reconcile_with_streams_and_ignore_duplicate_sequences() {
        let mut cursor = Cursor {
            turn: "operation".into(),
            ..Default::default()
        };
        frame(
            &json!({"type":"assistant-stream","frame":{"type":"start","attemptId":"attempt"}}),
            &mut cursor,
            "s",
        );
        let delta = frame(
            &json!({"type":"assistant-stream","frame":{"type":"chunk","chunk":{"type":"text-delta","index":0,"text":"hello"}}}),
            &mut cursor,
            "s",
        );
        let event = json!({"type":"event","event":{"seq":0,"type":"assistant/message","data":{"message":{"content":[{"type":"text","text":"hello"}]}}}});
        let committed = frame(&event, &mut cursor, "s");
        assert_eq!(
            delta[0]["params"]["itemId"],
            committed[0]["params"]["item"]["id"]
        );
        assert!(frame(&event, &mut cursor, "s").is_empty());
    }
    #[test]
    fn unknown_turn_end_cannot_complete_another_admitted_prompt() {
        let mut cursor = Cursor {
            turn: "new-prompt".into(),
            native_turn: Some(2),
            ..Default::default()
        };
        let event = json!({"type":"event","event":{"seq":4,"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}});
        assert!(frame(&event, &mut cursor, "s").is_empty());
        assert_eq!(cursor.turn, "new-prompt");
    }
    #[test]
    fn cancellation_failure_and_token_limits_have_distinct_terminal_results() {
        for (reason, expected) in [
            ("completed", "completed"),
            ("error", "failed"),
            ("aborted", "interrupted"),
            ("max-tokens", "failed"),
        ] {
            let mut cursor = Cursor {
                turn: "prompt".into(),
                native_turn: Some(1),
                ..Default::default()
            };
            let output = frame(
                &json!({"type":"event","event":{"seq":3,"type":"turn/end","data":{"turn":1,"reason":{"kind":reason}}}}),
                &mut cursor,
                "s",
            );
            assert_eq!(output[0]["params"]["turn"]["status"], expected);
            assert!(cursor.turn.is_empty());
        }
    }
    #[test]
    fn history_turn_ids_follow_native_records_across_rounds_and_page_boundaries() {
        let records = json!([
            {"event":{"seq":10,"type":"assistant/message","data":{"turn":7,"content":[{"type":"text","text":"prior reply"}]}}},
            {"event":{"seq":11,"type":"turn/start","data":{"turn":8}}},
            {"event":{"seq":12,"type":"user/message","data":{"turn":8,"content":[{"type":"text","text":"next question"}]}}},
            {"event":{"seq":13,"type":"assistant/message","data":{"turn":8,"content":[{"type":"text","text":"next reply"}]}}},
            {"event":{"seq":14,"type":"turn/start","data":{"turn":9}}},
            {"event":{"seq":15,"type":"assistant/message","data":{"turn":9,"content":[{"type":"text","text":"last reply"}]}}}
        ]);
        let items = super::history_records(records.as_array().unwrap(), "s");
        assert_eq!(
            items
                .iter()
                .map(|v| v["turnId"].clone())
                .collect::<Vec<_>>(),
            json!(["s-7", "s-8", "s-8", "s-9"])
                .as_array()
                .unwrap()
                .clone()
        );
        let tail = super::history_records(&records.as_array().unwrap()[3..], "s");
        assert_eq!(tail[0]["turnId"], items[2]["turnId"]);
    }
    #[test]
    fn native_archive_eligibility_does_not_require_attachment_ownership() {
        for (running, archived, expected) in [
            (false, false, true),
            (true, false, false),
            (false, true, false),
        ] {
            let projected =
                super::session(&json!({"sessionId":"s","running":running}), false, archived);
            assert_eq!(projected["archiveAllowed"], expected);
            assert_eq!(projected["owned"], false);
        }
    }
    #[test]
    fn history_reads_compact_tools_without_showing_replacement_surfaces_as_new_messages() {
        let records = json!([
            {"type":"event","event":{"seq":0,"type":"user/message","surfaceOp":{"op":"replace","startSeq":0,"endSeq":1},"data":{"content":[{"type":"text","text":"replacement"}]}}},
            {"type":"event","event":{"seq":1,"type":"assistant/message","data":{"message":{"content":[{"type":"tool-call","id":"tool","name":"read_file","arguments":"{\"path\":\"src/main.rs\"}"}]}}}}
        ]);
        let items = super::records(records.as_array().unwrap(), &mut Cursor::default(), "s");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "dynamicToolCall");
        let summary = crate::harness::presentation::item(&items[0]).unwrap();
        assert_eq!(summary["paths"], json!(["src/main.rs"]));
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    fn event(cursor: &mut Cursor, seq: u64, kind: &str, data: Value) -> Vec<Value> {
        frame(
            &json!({"type":"event","event":{"seq":seq,"type":kind,"data":data}}),
            cursor,
            "s",
        )
    }
    #[test]
    fn an_identified_turn_cannot_be_rebound_by_a_later_user_message() {
        let mut cursor = Cursor {
            turn: "owned".into(),
            expected_request: Some("owned".into()),
            ..Default::default()
        };
        event(&mut cursor, 0, "turn/start", json!({"turn":1}));
        event(
            &mut cursor,
            1,
            "user/message",
            json!({"turn":1,"source":{"kind":"user","rpcId":"owned"},"content":[{"type":"text","text":"input"}]}),
        );
        assert_eq!(cursor.native_turn, Some(1));
        event(
            &mut cursor,
            2,
            "user/message",
            json!({"turn":2,"source":{"kind":"user","rpcId":"owned"},"content":[{"type":"text","text":"input"}]}),
        );
        assert_eq!(cursor.native_turn, Some(1));
    }
    #[test]
    fn only_the_identified_prompt_turn_can_complete_the_owned_operation() {
        let mut cursor = Cursor {
            turn: "owned".into(),
            expected_request: Some("owned".into()),
            ..Default::default()
        };
        assert!(event(&mut cursor, 0, "turn/start", json!({"turn":1})).is_empty());
        let foreign = event(
            &mut cursor,
            1,
            "user/message",
            json!({"turn":1,"source":{"kind":"user","rpcId":"foreign"},"content":[{"type":"text","text":"foreign input"}]}),
        );
        assert_eq!(foreign[0]["params"]["turnId"], "s-1");
        assert!(event(
            &mut cursor,
            2,
            "turn/end",
            json!({"turn":1,"reason":{"kind":"completed"}})
        )
        .is_empty());
        assert_eq!(cursor.turn, "owned");
        event(&mut cursor, 3, "turn/start", json!({"turn":2}));
        let owned = event(
            &mut cursor,
            4,
            "user/message",
            json!({"turn":2,"source":{"kind":"user","rpcId":"owned"},"content":[{"type":"text","text":"owned input"}]}),
        );
        assert!(owned
            .iter()
            .any(|message| message["method"] == "turn/started"
                && message["params"]["turn"]["id"] == "owned"));
        cursor.control_pending = Some("cancel-request".into());
        let terminal = event(
            &mut cursor,
            5,
            "turn/end",
            json!({"turn":2,"reason":{"kind":"completed"}}),
        );
        assert_eq!(terminal[0]["params"]["turn"]["id"], "owned");
        assert_eq!(terminal[0]["params"]["turn"]["status"], "completed");
        assert!(
            cursor.busy(),
            "an unconfirmed control receipt must retain its own barrier"
        );
    }
    #[test]
    fn identifies_turn_from_native_user_message_without_a_turn_field() {
        let mut cursor = Cursor {
            turn: "owned".into(),
            expected_request: Some("owned".into()),
            admission: Some(super::super::admission::PromptAdmission::new(
                "owned", "input",
            )),
            ..Default::default()
        };
        event(&mut cursor, 0, "turn/start", json!({"turn":1}));
        let started = event(
            &mut cursor,
            1,
            "user/message",
            json!({"role":"user","source":{"kind":"user","rpcId":"owned"},"content":[{"type":"text","text":"input"}]}),
        );
        assert_eq!(cursor.native_turn, Some(1));
        assert!(cursor.admission.as_ref().unwrap().accepted);
        assert!(started
            .iter()
            .any(|message| message["method"] == "turn/started"));
        let done = event(
            &mut cursor,
            2,
            "turn/end",
            json!({"turn":1,"reason":{"kind":"completed"}}),
        );
        assert!(done
            .iter()
            .any(|message| message["method"] == "turn/completed"
                && message["params"]["turn"]["id"] == "owned"));
        assert!(cursor.turn.is_empty());
    }

    #[test]
    fn a_closed_native_turn_is_not_reused_for_a_later_unbound_prompt() {
        let mut cursor = Cursor {
            turn: "owned".into(),
            expected_request: Some("owned".into()),
            observed_native: Some(1),
            ..Default::default()
        };
        assert!(event(
            &mut cursor,
            0,
            "turn/end",
            json!({"turn":1,"reason":{"kind":"completed"}})
        )
        .is_empty());
        event(
            &mut cursor,
            1,
            "user/message",
            json!({"source":{"kind":"user","rpcId":"owned"},"content":[{"type":"text","text":"input"}]}),
        );
        assert_eq!(cursor.native_turn, None);
        assert_eq!(cursor.turn, "owned");
    }
}
