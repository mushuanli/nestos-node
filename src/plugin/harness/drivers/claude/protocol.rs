//! Translate Claude SDK frames into the public harness event vocabulary.
use super::process::Context;
use crate::core::error::Error;
use serde_json::{json, Value};
pub fn consume(context: &mut Context, frame: Value) -> Result<(), Error> {
    match frame["type"].as_str() {
        Some("system") if frame["subtype"] == "init" => {
            current_model(context, &frame);
            let commands = super::commands::refresh(
                &context.commands.lock().unwrap(),
                &frame["slash_commands"],
            );
            *context.commands.lock().unwrap() = commands.clone();
            if frame["permissionMode"].is_string() {
                context.session.lock().unwrap()["permissionMode"] = frame["permissionMode"].clone();
            }
            push(
                context,
                "harness/commands/updated",
                json!({"threadId":context.id,"commands":commands}),
            )
        }
        Some("system") => super::system::consume(context, &frame),
        Some("user") if frame["parent_tool_use_id"].is_null() => acknowledge(context, &frame),
        Some("assistant") if frame["parent_tool_use_id"].is_null() => assistant(context, &frame),
        Some("rate_limit_event") => {
            if let Some(quota) = super::super::metrics::claude_quota(&frame["rate_limit_info"]) {
                let mut session = context.session.lock().unwrap();
                session["metrics"]["quota"] = quota;
                session["metrics"]["observedAt"] = json!(chrono::Utc::now().timestamp_millis());
            }
            Ok(())
        }
        Some("result") => complete(context, &frame),
        Some("control_request") => permission(context, frame),
        Some("control_cancel_request") => resolve(context, &frame["request_id"]),
        Some("stream_event") if frame["parent_tool_use_id"].is_null() => delta(context, &frame),
        _ => Ok(()),
    }
}
fn current_model(context: &Context, frame: &Value) {
    let Some(model) = frame["model"].as_str() else {
        return;
    };
    let mut catalog = context.models.lock().unwrap();
    let id = catalog["models"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["id"] == model || row["resolvedModel"] == model)
        })
        .map(|row| row["id"].clone());
    if let Some(id) = id {
        let mut current = if catalog["current"]["modelId"] == id {
            catalog["current"].clone()
        } else {
            json!({"modelId":id})
        };
        if frame["effort"].is_string() {
            current["reasoningEffort"] = frame["effort"].clone();
        }
        catalog["current"] = current;
    }
}
pub(super) fn push(context: &Context, method: &str, params: Value) -> Result<(), Error> {
    context
        .events
        .lock()
        .unwrap()
        .push(json!({"method":method,"params":params}))
}
fn acknowledge(context: &mut Context, frame: &Value) -> Result<(), Error> {
    if let Some(id) = frame["uuid"].as_str() {
        if let Some(target) = context.steering.remove(id) {
            return acknowledge_steer(context, frame, id, &target);
        }
        if id != context.turn
            && context
                .items
                .lock()
                .unwrap()
                .iter()
                .any(|item| item["id"] == id)
        {
            return Ok(());
        }
    }
    let Some(id) = frame["uuid"].as_str().filter(|id| *id == context.turn) else {
        return tool_results(context, frame);
    };
    update_title(context, &frame["message"]["content"]);
    if let Some(reply) = context.pending.remove(id) {
        let _ = reply.send(Ok(json!({"turnId":id})));
    }
    let item =
        json!({"id":id,"type":"userMessage","content":frame["message"]["content"],"turnId":id});
    context.remember(item.clone());
    push(
        context,
        "item/completed",
        json!({"threadId":context.id,"turnId":id,"item":item}),
    )
}
fn acknowledge_steer(
    context: &mut Context,
    frame: &Value,
    id: &str,
    target: &str,
) -> Result<(), Error> {
    if context.session.lock().unwrap()["status"] != "active" {
        context.turn = id.into();
        context.interrupted = false;
        context.command = false;
        context.command_output = false;
        {
            let mut session = context.session.lock().unwrap();
            session["status"] = json!("active");
            session["activeTurnId"] = json!(id);
        }
        push(
            context,
            "turn/started",
            json!({"threadId":context.id,"turn":{"id":id}}),
        )?;
    }
    let turn = context.turn.clone();
    if let Some(reply) = context.pending.remove(id) {
        let _ = reply.send(Ok(json!({"turnId":turn,"targetTurnId":target})));
    }
    update_title(context, &frame["message"]["content"]);
    let item =
        json!({"id":id,"type":"userMessage","content":frame["message"]["content"],"turnId":turn});
    context.remember(item.clone());
    push(
        context,
        "item/completed",
        json!({"threadId":context.id,"turnId":turn,"item":item}),
    )
}
fn update_title(context: &Context, content: &Value) {
    let mut session = context.session.lock().unwrap();
    if session["title"] == "" {
        session["title"] = json!(super::history::text(content)
            .chars()
            .take(512)
            .collect::<String>());
    }
    session["updatedAt"] = json!(chrono::Utc::now().timestamp_millis());
}
fn tool_results(context: &mut Context, frame: &Value) -> Result<(), Error> {
    for block in frame["message"]["content"].as_array().into_iter().flatten() {
        if block["type"] != "tool_result" {
            continue;
        }
        let item = context
            .items
            .lock()
            .unwrap()
            .iter()
            .find(|item| item["id"] == block["tool_use_id"])
            .cloned();
        if let Some(mut item) = item {
            item["status"] = json!(if block["is_error"] == true {
                "failed"
            } else {
                "completed"
            });
            context.remember(item.clone());
            push(
                context,
                "item/completed",
                json!({"threadId":context.id,"turnId":context.turn,"item":item}),
            )?;
        }
    }
    Ok(())
}
fn assistant(context: &mut Context, frame: &Value) -> Result<(), Error> {
    let uuid = frame["message"]["id"]
        .as_str()
        .or(frame["uuid"].as_str())
        .ok_or_else(Error::invalid)?;
    let blocks = frame["message"]["content"]
        .as_array()
        .ok_or_else(Error::invalid)?;
    for (index, block) in blocks.iter().enumerate() {
        if let Some(item) = item(block, &format!("{uuid}:{index}"), &context.turn) {
            let mut item = item;
            let method = if block["type"] == "tool_use" {
                item["status"] = json!("inProgress");
                "item/started"
            } else {
                "item/completed"
            };
            context.remember(item.clone());
            push(
                context,
                method,
                json!({"threadId":context.id,"turnId":context.turn,"item":item}),
            )?;
        }
    }
    Ok(())
}
pub fn item(block: &Value, id: &str, turn: &str) -> Option<Value> {
    let mut value = match block["type"].as_str()? {
        "text" => json!({"id":id,"type":"agentMessage","text":block["text"]}),
        "thinking" => json!({"id":id,"type":"reasoning","text":block["thinking"]}),
        "tool_use" => {
            json!({"id":block["id"],"type":"functionCall","name":block["name"],"arguments":block["input"].to_string()})
        }
        _ => return None,
    };
    value["turnId"] = if turn.is_empty() {
        Value::Null
    } else {
        json!(turn)
    };
    value["status"] = json!("completed");
    value["timestamp"] = super::super::presentation::time_ms(&block["timestamp"]);
    super::super::presentation::item(&value)
}
fn complete(context: &mut Context, frame: &Value) -> Result<(), Error> {
    if context.command && !context.command_output {
        if let Some(output) = frame["result"].as_str().filter(|text| !text.is_empty()) {
            super::system::note(context, "result", output)?;
        }
    }
    // Native control-like commands may finish without echoing a user frame.
    if let Some(reply) = context.pending.remove(&context.turn) {
        let _ = reply.send(Ok(json!({"turnId":context.turn})));
    }
    let mut session = context.session.lock().unwrap();
    if let Some(cost) = super::super::metrics::cost(&frame["total_cost_usd"]) {
        session["metrics"]["cost"] = cost;
        session["metrics"]["observedAt"] = json!(chrono::Utc::now().timestamp_millis());
    }
    session["status"] = json!("idle");
    session["activeTurnId"] = Value::Null;
    let result = if context.interrupted {
        "cancelled"
    } else if frame["is_error"].as_bool() == Some(true) {
        "failed"
    } else {
        "completed"
    };
    session["lastTurnResult"] = json!(result);
    session["lastTurnId"] = json!(context.turn);
    session["updatedAt"] = json!(chrono::Utc::now().timestamp_millis());
    drop(session);
    let requests = context.requests.keys().cloned().collect::<Vec<_>>();
    for request in requests {
        resolve(context, &json!(request))?;
    }
    push(
        context,
        "turn/completed",
        json!({"threadId":context.id,"turn":{"id":context.turn,"status":if result == "cancelled" {"interrupted"} else {result}}}),
    )
}
fn resolve(context: &mut Context, id: &Value) -> Result<(), Error> {
    if let Some(id) = id.as_str() {
        context.requests.remove(id);
    }
    push(context, "serverRequest/resolved", json!({"requestId":id}))
}
fn permission(context: &mut Context, frame: Value) -> Result<(), Error> {
    if frame["request"]["subtype"] != "can_use_tool" {
        return Err(Error::unsupported());
    }
    let id = frame["request_id"]
        .as_str()
        .ok_or_else(Error::invalid)?
        .to_owned();
    let request = &frame["request"];
    let tool = request["tool_name"].as_str().unwrap_or("");
    let input = tool == "AskUserQuestion";
    let questions=request["input"]["questions"].as_array().map(|qs|qs.iter().enumerate().map(|(i,q)|json!({"id":i.to_string(),"question":q["question"],"header":q["header"],"multiSelect":q["multiSelect"].as_bool().unwrap_or(false),"isOther":true,"options":q["options"]})).collect::<Vec<_>>());
    let method = if input {
        "item/tool/requestUserInput"
    } else if ["Edit", "Write", "NotebookEdit"].contains(&tool) {
        "item/fileChange/requestApproval"
    } else {
        "item/commandExecution/requestApproval"
    };
    context.events.lock().unwrap().push(json!({"id":id,"method":method,"params":{"threadId":context.id,"turnId":context.turn,"toolName":tool,"command":request["input"]["command"],"path":request["input"]["file_path"],"questions":questions,"decisions":["accept","decline","cancel"]}}))?;
    context.requests.insert(id, frame);
    Ok(())
}
fn delta(context: &mut Context, frame: &Value) -> Result<(), Error> {
    let event = &frame["event"];
    if event["type"] == "message_start" {
        context.stream = event["message"]["id"].as_str().unwrap_or("").into();
    }
    if event["type"] != "content_block_delta" || event["delta"]["type"] != "text_delta" {
        return Ok(());
    }
    push(
        context,
        "item/agentMessage/delta",
        json!({"threadId":context.id,"turnId":context.turn,"itemId":format!("{}:{}",context.stream,event["index"].as_u64().unwrap_or(0)),"delta":event["delta"]["text"]}),
    )
}
