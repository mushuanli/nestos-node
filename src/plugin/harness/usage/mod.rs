//! Recoverable native transcript metering; prices and billing policy live in the host.
mod claude;
mod codex;
use crate::core::error::Error;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    sync::{Arc, Mutex},
};

const MAX_READ: usize = 32 * 1024 * 1024;
const MAX_RECORD: usize = 2 * 1024 * 1024;
const MAX_ENTRIES: usize = 8192;
static READS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
pub(super) fn permit() -> Result<tokio::sync::SemaphorePermit<'static>, Error> {
    READS
        .try_acquire()
        .map_err(|_| Error::too_many("HARNESS_USAGE_BUSY"))
}
#[derive(Clone, Copy)]
pub(super) enum Kind {
    Codex,
    Claude,
}
#[derive(Default)]
pub(super) struct Reader(Mutex<BTreeMap<String, Arc<Mutex<Cursor>>>>);
#[derive(Clone, Default, Serialize, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
struct Tokens {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    id: String,
    turn_id: Option<String>,
    provider_id: Option<String>,
    model_id: Option<String>,
    tokens: Tokens,
}
#[derive(Default)]
struct Cursor {
    identity: (u64, u64),
    offset: u64,
    modified: i128,
    pending: Vec<u8>,
    skipped: bool,
    incomplete: bool,
    turn: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    cumulative: Option<Tokens>,
    context_window: Option<Value>,
    entries: BTreeMap<String, Entry>,
}
impl Reader {
    pub fn read(
        &self,
        file: File,
        session: &str,
        kind: Kind,
        authorize: impl Fn(&Value) -> Result<(), Error>,
    ) -> Result<Value, Error> {
        let cursor = {
            let mut cursors = self.0.lock().unwrap();
            if !cursors.contains_key(session) && cursors.len() >= 32 {
                let idle = cursors
                    .iter()
                    .find(|(_, cursor)| Arc::strong_count(cursor) == 1)
                    .map(|(id, _)| id.clone());
                match idle {
                    Some(id) => {
                        cursors.remove(&id);
                    }
                    None => return Err(Error::too_many("HARNESS_USAGE_BUSY")),
                }
            }
            cursors.entry(session.to_owned()).or_default().clone()
        };
        let mut cursor = cursor.lock().unwrap();
        let result = cursor.read_file(file, session, kind, authorize);
        if result.is_err() {
            *cursor = Cursor::default();
        }
        result
    }
}
impl Cursor {
    fn read_file(
        &mut self,
        mut file: File,
        session: &str,
        kind: Kind,
        authorize: impl Fn(&Value) -> Result<(), Error>,
    ) -> Result<Value, Error> {
        let metadata = file.metadata()?;
        let modified = self.prepare(&metadata);
        file.seek(SeekFrom::Start(self.offset))?;
        let budget = metadata
            .len()
            .saturating_sub(self.offset)
            .min(MAX_READ as u64);
        let mut bytes = Vec::new();
        file.take(budget).read_to_end(&mut bytes)?;
        self.consume(&bytes, kind, authorize)?;
        self.modified = modified;
        let snapshot = self.snapshot(session, metadata.len());
        if snapshot["totals"]
            .as_object()
            .unwrap()
            .values()
            .any(|v| v.as_u64().unwrap_or(u64::MAX) > 9_007_199_254_740_991)
        {
            return Err(Error::too_large("HARNESS_USAGE_LIMIT"));
        }
        Ok(snapshot)
    }
    fn prepare(&mut self, metadata: &std::fs::Metadata) -> i128 {
        let identity = (metadata.dev(), metadata.ino());
        let modified = metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128;
        if self.identity != identity
            || metadata.len() < self.offset
            || metadata.len() == self.offset && modified != self.modified
        {
            *self = Cursor {
                identity,
                ..Cursor::default()
            };
        }
        modified
    }
    fn consume(
        &mut self,
        bytes: &[u8],
        kind: Kind,
        authorize: impl Fn(&Value) -> Result<(), Error>,
    ) -> Result<(), Error> {
        for part in bytes.split_inclusive(|b| *b == b'\n') {
            if self.pending.len() + part.len() > MAX_RECORD {
                self.pending.clear();
                self.skipped = true;
                self.incomplete = true;
            }
            if !self.skipped {
                self.pending.extend_from_slice(part);
            }
            self.offset += part.len() as u64;
            if part.last() != Some(&b'\n') {
                continue;
            }
            self.consume_line(kind, &authorize)?;
        }
        Ok(())
    }
    fn consume_line(
        &mut self,
        kind: Kind,
        authorize: &impl Fn(&Value) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let line = std::mem::take(&mut self.pending);
        if self.skipped {
            self.skipped = false;
            return Ok(());
        }
        match serde_json::from_slice::<Value>(&line) {
            Ok(record) => {
                authorize(&record)?;
                match kind {
                    Kind::Codex => self.codex(&record),
                    Kind::Claude => self.claude(&record),
                }
            }
            Err(_) => {
                self.incomplete = true;
                Ok(())
            }
        }
    }
    fn snapshot(&self, session: &str, length: u64) -> Value {
        let mut total = Tokens::default();
        for entry in self.entries.values() {
            total.add(&entry.tokens);
        }
        json!({"sessionId":session,"scope":"transcript","source":"native-history",
            "revision":format!("{}:{}:{}:{}",self.identity.0,self.identity.1,self.offset,self.modified),
            "complete":!self.entries.is_empty() && !self.incomplete && self.offset==length && self.pending.is_empty() && !self.skipped,
            "contextWindow":self.context_window,"observedAt":chrono::Utc::now().timestamp_millis(),"totals":total,"entries":self.entries.values().collect::<Vec<_>>()})
    }
    fn put(&mut self, id: String, tokens: Tokens, replace: bool) -> Result<(), Error> {
        if !self.entries.contains_key(&id) && self.entries.len() >= MAX_ENTRIES {
            return Err(Error::too_large("HARNESS_USAGE_LIMIT"));
        }
        let entry = self.entries.entry(id.clone()).or_insert_with(|| Entry {
            id,
            turn_id: self.turn.clone(),
            provider_id: self.provider.clone(),
            model_id: self.model.clone(),
            tokens: Tokens::default(),
        });
        if replace {
            entry.tokens = tokens;
        } else {
            entry.tokens.add(&tokens);
        }
        Ok(())
    }
}
impl Tokens {
    fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
    }
    fn subtract(&self, other: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(other.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(other.output_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_sub(other.cache_read_tokens),
            cache_write_tokens: self
                .cache_write_tokens
                .saturating_sub(other.cache_write_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_sub(other.reasoning_tokens),
        }
    }
}
fn count(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}
fn valid_counts(value: &Value, optional: &[&str]) -> bool {
    let valid = |v: &Value| v.as_u64().is_some_and(|n| n <= 9_007_199_254_740_991);
    ["input_tokens", "output_tokens"]
        .iter()
        .all(|key| valid(&value[*key]))
        && optional.iter().all(|key| value.get(*key).is_none_or(valid))
}
fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 512)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests;
pub(super) fn attach(session: &mut Value, result: Result<Value, Error>) {
    match result {
        Ok(usage) => {
            session["usage"] = usage;
            session["usageUnavailable"] = json!(false);
        }
        Err(error) => {
            session["usageUnavailable"] = json!(true);
            if error.code != "ENOENT" {
                crate::core::events::emit(
                    crate::core::events::Level::Warn,
                    "harness.usage.unavailable",
                    json!({"sessionId":session["id"],"code":error.code}),
                );
            }
        }
    }
}
