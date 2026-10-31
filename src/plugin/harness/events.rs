//! Bounded replay buffer and pending native interaction requests.
use crate::core::error::Error;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};

const MAX_EVENTS: usize = 1024;
const MAX_BYTES: usize = 8 * 1024 * 1024;
pub struct Events {
    commands: super::command_runs::CommandRuns,
    changed: tokio::sync::watch::Sender<u64>,
    pub epoch: String,
    pub runtime: Option<std::sync::Arc<crate::projects::runtime::ProjectRuntime>>,
    next: u64,
    bytes: usize,
    entries: VecDeque<(u64, Value, usize, i64)>,
    pub requests: HashMap<String, Value>,
    active: std::collections::HashSet<String>,
    last_activity: std::time::Instant,
}

impl Events {
    pub fn new() -> Self {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).expect("random unavailable");
        Self {
            commands: Default::default(),
            changed: tokio::sync::watch::channel(0).0,
            epoch: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            runtime: None,
            next: 0,
            bytes: 0,
            entries: VecDeque::new(),
            requests: HashMap::new(),
            active: std::collections::HashSet::new(),
            last_activity: std::time::Instant::now(),
        }
    }
    pub fn push(&mut self, message: Value) -> Result<(), Error> {
        let size = message.to_string().len();
        if size > MAX_BYTES {
            return Err(Error::too_large("EFBIG"));
        }
        self.track(&message)?;
        let command = self.commands.observe(&message);
        self.last_activity = std::time::Instant::now();
        if let Some(runtime) = &self.runtime {
            runtime.event(&message);
        }
        super::diagnostics::record(&message);
        self.next += 1;
        self.bytes += size;
        self.entries.push_back((
            self.next,
            message,
            size,
            chrono::Utc::now().timestamp_millis(),
        ));
        while self.entries.len() > MAX_EVENTS || self.bytes > MAX_BYTES {
            if let Some((_, _, size, _)) = self.entries.pop_front() {
                self.bytes -= size;
            }
        }
        self.changed.send_replace(self.next);
        if let Some(command) = command {
            if command["params"]["status"] == "running" {
                if let Some(runtime) = &self.runtime {
                    runtime.bind_turn(
                        command["params"]["threadId"].as_str().unwrap(),
                        command["params"]["commandId"].as_str().unwrap(),
                        command["params"]["turnId"].as_str().unwrap(),
                    );
                }
            }
            self.push(command)?;
        }
        Ok(())
    }
    pub fn register_command(&mut self, thread: &str, operation: &str) -> Result<(), Error> {
        self.commands.register(thread, operation)
    }
    pub fn forget_command(&mut self, thread: &str, operation: &str) {
        self.commands.forget(thread, operation);
    }
    pub fn context_usage(&self, thread: &str) -> Option<Value> {
        let (_, event, _, at) = self.entries.iter().rev().find(|(_, event, _, _)| {
            event["method"] == "thread/tokenUsage/updated" && event["params"]["threadId"] == thread
        })?;
        let mut value = super::metrics::codex_context(&event["params"]["tokenUsage"]).unwrap_or(Value::Null);
        if value.is_object() {value["observedAt"] = json!(at); }
        Some(value)
    }
    pub fn runtime_metadata(&self, thread: &str) -> Value {
        let mut value = json!({});
        let mut lifecycle = false;
        for (_, event, _, _) in self.entries.iter().rev() {
            if event["params"]["threadId"] != thread {
                continue;
            }
            let turn = &event["params"]["turn"];
            match event["method"].as_str() {
                Some("turn/started") if !lifecycle => {
                    value["activeTurnId"] = turn["id"].clone();
                    lifecycle = true;
                }
                Some("turn/completed") if value["lastTurnId"].is_null() => {
                    lifecycle = true;
                    value["lastTurnId"] = turn["id"].clone();
                    value["lastTurnResult"] = match turn["status"].as_str() {
                        Some("completed") => json!("completed"),
                        Some("failed") => json!("failed"),
                        Some("interrupted" | "cancelled") => json!("cancelled"),
                        _ => Value::Null,
                    };
                }
                _ => (),
            }
        }
        value
    }
    fn track(&mut self, message: &Value) -> Result<(), Error> {
        if let Some(id) = message["params"]["threadId"].as_str() {
            match message["method"].as_str() {
                Some("turn/started") => {
                    self.active.insert(id.into());
                }
                Some("turn/completed" | "harness/disconnected") => {
                    self.active.remove(id);
                }
                Some("thread/status/changed") => {
                    if message["params"]["status"]["type"] == "active" {
                        self.active.insert(id.into());
                    } else if matches!(
                        message["params"]["status"]["type"].as_str(),
                        Some("idle" | "notLoaded")
                    ) {
                        self.active.remove(id);
                    }
                }
                _ => (),
            }
        }
        if message.get("method").is_some() && message.get("id").is_some() {
            let retained: usize = self
                .requests
                .values()
                .map(|value| value.to_string().len())
                .sum();
            if retained + message.to_string().len() > MAX_BYTES {
                return Err(Error::too_large("EFBIG"));
            }
            if self.requests.len() >= 128 {
                return Err(Error::too_many("HARNESS_REQUEST_LIMIT"));
            }
            self.requests
                .insert(message["id"].to_string(), message.clone());
        }
        if message["method"] == "serverRequest/resolved" {
            self.requests
                .remove(&message["params"]["requestId"].to_string());
        }
        Ok(())
    }
    pub fn page(&self, epoch: Option<&str>, after: u64) -> Value {
        let first = self.entries.front().map_or(self.next + 1, |e| e.0);
        let reset = epoch.is_some_and(|e| e != self.epoch) || after > self.next;
        let gap = reset || after.saturating_add(1) < first;
        let events: Vec<_> = self
            .entries
            .iter()
            .filter(|e| reset || e.0 > after)
            .take(128)
            .map(|(seq, message, _, timestamp)| json!({"seq": seq, "message": message, "timestamp": timestamp}))
            .collect();
        let cursor = events
            .last()
            .and_then(|e| e["seq"].as_u64())
            .unwrap_or(self.next);
        json!({"epoch":self.epoch,"cursor":cursor,"after":if gap {first.saturating_sub(1)}else{after},"gap":gap,"events":events,
            "requests":self.requests.values().collect::<Vec<_>>()})
    }
    pub fn position(&self) -> Value {
        json!({"after":self.next,"eventEpoch":self.epoch})
    }
    pub fn thread_idle(&self, id: &str) -> bool {
        !self.active.contains(id)
            && !self
                .requests
                .values()
                .any(|request| request["params"]["threadId"] == id)
    }
    pub fn idle_since(&self) -> Option<std::time::Instant> {
        (self.active.is_empty() && self.requests.is_empty()).then_some(self.last_activity)
    }
}

/// A stream retains only the buffer's weak identity, never its native driver.
pub(super) fn stream(
    events: &std::sync::Arc<std::sync::Mutex<Events>>,
    args: Value,
) -> crate::plugin::common::EventStream {
    use futures_util::stream;
    let receiver = events.lock().unwrap().changed.subscribe();
    let state = (std::sync::Arc::downgrade(events), receiver, args, true);
    Box::pin(stream::unfold(
        state,
        |(weak, mut receiver, mut cursor, ready)| async move {
            if !ready && receiver.changed().await.is_err() {
                return None;
            }
            receiver.borrow_and_update();
            let Some(events) = weak.upgrade() else {
                return None;
            };
            let page = events.lock().unwrap().page(
                cursor["eventEpoch"].as_str(),
                cursor["after"].as_u64().unwrap_or(0),
            );
            cursor["after"] = page["cursor"].clone();
            cursor["eventEpoch"] = page["epoch"].clone();
            let more = page["events"]
                .as_array()
                .is_some_and(|rows| rows.len() == 128);
            Some((
                Ok(super::presentation::events(page)),
                (weak, receiver, cursor, more),
            ))
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn streams_full_pages_immediately_then_sleeps_until_a_real_event() {
        use futures_util::StreamExt;
        let events = std::sync::Arc::new(std::sync::Mutex::new(Events::new()));
        for index in 0..300 {
            events
                .lock()
                .unwrap()
                .push(json!({"method":"item/delta","params":{"index":index}}))
                .unwrap();
        }
        let mut source = stream(&events, json!({}));
        assert_eq!(
            source.next().await.unwrap().unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            128
        );
        assert_eq!(
            source.next().await.unwrap().unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            128
        );
        assert_eq!(
            source.next().await.unwrap().unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            44
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), source.next())
                .await
                .is_err()
        );
        events
            .lock()
            .unwrap()
            .push(json!({"method":"turn/completed","params":{"threadId":"s"}}))
            .unwrap();
        let page = tokio::time::timeout(std::time::Duration::from_secs(1), source.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(page["events"][0]["message"]["method"], "turn/completed");
        assert_eq!(std::sync::Arc::strong_count(&events), 1);
        drop(events);
        assert!(source.next().await.is_none());
    }
    #[test]
    fn execution_and_pending_dialogs_pin_the_runtime_until_explicit_resolution() {
        let mut events = Events::new();
        assert!(events.idle_since().is_some());
        events
            .push(json!({"method":"turn/started","params":{"threadId":"s"}}))
            .unwrap();
        assert!(events.idle_since().is_none());
        events
            .push(json!({"id":"q","method":"item/tool/requestUserInput","params":{"threadId":"s"}}))
            .unwrap();
        events
            .push(json!({"method":"turn/completed","params":{"threadId":"s"}}))
            .unwrap();
        assert!(events.idle_since().is_none());
        events
            .push(json!({"method":"serverRequest/resolved","params":{"requestId":"q"}}))
            .unwrap();
        assert!(events.idle_since().is_some());
    }
    #[test]
    fn reports_replay_gaps_and_epoch_reset() {
        let mut events = Events::new();
        for _ in 0..MAX_EVENTS + 1 {
            events
                .push(json!({"method":"item/delta","params":{}}))
                .unwrap();
        }
        let page = events.page(Some(&events.epoch), 0);
        assert_eq!(page["gap"], true);
        assert_eq!(page["events"][0]["seq"], 2);
        assert_eq!(page["cursor"], 129);
        assert_eq!(events.page(Some("old"), 1025)["gap"], true);
    }
    #[test]
    fn preserves_pending_requests_after_replay_eviction_and_resolves_them() {
        let mut events = Events::new();
        events
            .push(json!({"method":"approval","id":"pending","params":{}}))
            .unwrap();
        for _ in 0..MAX_EVENTS {
            events.push(json!({"method":"delta"})).unwrap();
        }
        assert_eq!(
            events.page(None, 0)["requests"].as_array().unwrap().len(),
            1
        );
        events
            .push(json!({"method":"serverRequest/resolved","params":{"requestId":"pending"}}))
            .unwrap();
        assert!(events.requests.is_empty());
    }
    #[test]
    fn bounds_pending_request_bytes_independently_from_replay() {
        let mut events = Events::new();
        events
            .push(json!({"method":"approval","id":1,"params":{"text":"x".repeat(MAX_BYTES / 2)}}))
            .unwrap();
        assert_eq!(
            events
                .push(
                    json!({"method":"approval","id":2,"params":{"text":"x".repeat(MAX_BYTES / 2)}})
                )
                .unwrap_err()
                .code,
            "EFBIG"
        );
        assert_eq!(events.requests.len(), 1);
    }
}
