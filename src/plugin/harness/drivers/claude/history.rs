//! Read-only Claude transcripts, scoped by verified cwd rather than encoded folder names.
use crate::{
    core::error::Error,
    fs::Export,
    harness::{config::ProfileConfig, presentation},
    projects::runtime::ProjectRuntime,
};
use serde_json::{json, Value};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::Path,
    time::Instant,
};
const MAX_BYTES: usize = 16 * 1024 * 1024;
pub struct Catalog {
    pub entries: Vec<Entry>,
}
pub struct Entry {
    pub session: Value,
    pub path: String,
    identity: (u64, u64),
}
pub fn uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}
pub fn cwd_allowed(config: &ProfileConfig, runtime: Option<&ProjectRuntime>, cwd: &str) -> bool {
    runtime.map_or_else(
        || {
            Path::new(cwd)
                .canonicalize()
                .ok()
                .is_some_and(|path| config.workspaces.iter().any(|w| path.starts_with(&w.path)))
        },
        |r| r.resolve_cwd(cwd).is_ok(),
    )
}
fn keys(config: &ProfileConfig, runtime: Option<&ProjectRuntime>) -> Result<Vec<String>, Error> {
    let mut paths = config
        .workspaces
        .iter()
        .map(|w| w.path.clone())
        .collect::<Vec<_>>();
    if let Some(runtime) = runtime {
        paths.push(runtime.cwd().into());
        let state = runtime.state()?;
        let export = state
            .exports
            .get(&runtime.project.alias)
            .ok_or_else(Error::invalid)?;
        let file = export.open_dir(&runtime.project.path)?;
        paths.push(std::fs::read_link(format!(
            "/proc/self/fd/{}",
            file.as_raw_fd()
        ))?);
    }
    Ok(paths
        .iter()
        .filter_map(|p| p.to_str())
        .map(|p| {
            p.chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .take(200)
                .collect()
        })
        .collect())
}
struct Scan {
    entries: Vec<Entry>,
    bytes: usize,
    inspected: usize,
    started: Instant,
}
impl Scan {
    fn check(&self) -> Result<(), Error> {
        if self.inspected > 8192 || self.bytes > MAX_BYTES || self.started.elapsed().as_secs() >= 5
        {
            return Err(Error::too_large("CLAUDE_CATALOG_LIMIT"));
        }
        Ok(())
    }
    fn folder(
        &mut self,
        home: &Export,
        name: &str,
        config: &ProfileConfig,
        runtime: Option<&ProjectRuntime>,
    ) -> Result<(), Error> {
        let dir = home.open_dir(&format!("projects/{name}"))?;
        for file in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            self.inspected += 1;
            self.check()?;
            let file = file?;
            let filename = file.file_name();
            let Some(filename) = filename.to_str() else {
                continue;
            };
            let Some(id) = filename.strip_suffix(".jsonl").filter(|id| uuid(id)) else {
                continue;
            };
            if !file.file_type()?.is_file() {
                continue;
            }
            self.inspect(
                home,
                format!("projects/{name}/{filename}"),
                id,
                config,
                runtime,
            )?;
        }
        Ok(())
    }
    fn inspect(
        &mut self,
        home: &Export,
        path: String,
        id: &str,
        config: &ProfileConfig,
        runtime: Option<&ProjectRuntime>,
    ) -> Result<(), Error> {
        let (mut file, _) = home.read(&path)?;
        let identity = identity(&file)?;
        let (session, size) = metadata(&mut file, id, config, runtime)?;
        self.bytes += size;
        self.check()?;
        if session["cwd"]
            .as_str()
            .is_some_and(|cwd| cwd_allowed(config, runtime, cwd))
        {
            self.entries.push(Entry {
                session,
                path,
                identity,
            });
        }
        Ok(())
    }
    fn projects(
        &mut self,
        home: &Export,
        directory: &File,
        keys: &[String],
        config: &ProfileConfig,
        runtime: Option<&ProjectRuntime>,
    ) -> Result<(), Error> {
        for (count, folder) in
            std::fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))?.enumerate()
        {
            if count >= 2048 {
                return Err(Error::too_large("CLAUDE_CATALOG_LIMIT"));
            }
            self.check()?;
            let folder = folder?;
            let name = folder.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if folder.file_type()?.is_dir()
                && keys
                    .iter()
                    .any(|key| name == key || name.starts_with(&(key.clone() + "-")))
            {
                self.folder(home, name, config, runtime)?;
            }
        }
        Ok(())
    }
}
pub fn catalog(config: &ProfileConfig, runtime: Option<&ProjectRuntime>) -> Result<Catalog, Error> {
    let home = Export::open(&config.home)?;
    let keys = keys(config, runtime)?;
    let directory = match home.open_dir("projects") {
        Ok(dir) => dir,
        Err(error) if error.is_not_found() => {
            return Ok(Catalog {
                entries: Vec::new(),
            })
        }
        Err(error) => return Err(error),
    };
    let mut scan = Scan {
        entries: Vec::new(),
        bytes: 0,
        inspected: 0,
        started: Instant::now(),
    };
    scan.projects(&home, &directory, &keys, config, runtime)?;
    scan.entries.sort_by(|a, b| {
        b.session["updatedAt"]
            .as_i64()
            .cmp(&a.session["updatedAt"].as_i64())
            .then(a.session["id"].as_str().cmp(&b.session["id"].as_str()))
    });
    Ok(Catalog {
        entries: scan.entries,
    })
}
fn metadata(
    file: &mut File,
    id: &str,
    config: &ProfileConfig,
    runtime: Option<&ProjectRuntime>,
) -> Result<(Value, usize), Error> {
    let (size, prefix, tail) = metadata_window(file)?;
    let mut session = json!({"id":id,"title":"","cwd":null,"status":"notLoaded","createdAt":null,"updatedAt":null,"resumable":true,"owned":false,"archived":false});
    for (_, record) in lines(&prefix, 0)?
        .into_iter()
        .chain(lines(&tail, size.saturating_sub(65536))?)
    {
        if record["cwd"]
            .as_str()
            .is_some_and(|cwd| !cwd_allowed(config, runtime, cwd))
        {
            session["cwd"] = Value::Null;
            return Ok((session, prefix.len() + tail.len()));
        }
        fold_metadata(&mut session, &record, id)?;
    }
    session["updatedAt"] = file
        .metadata()?
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(Value::Null, |time| json!(time.as_millis() as u64));
    if session["cwd"].is_null() && size >= 65536 {
        return Err(Error::too_large("CLAUDE_METADATA_LIMIT"));
    }
    Ok((session, prefix.len() + tail.len()))
}
fn metadata_window(file: &mut File) -> Result<(u64, Vec<u8>, Vec<u8>), Error> {
    let size = file.metadata()?.len();
    let mut prefix = Vec::new();
    (&mut *file).take(65536).read_to_end(&mut prefix)?;
    let mut tail = Vec::new();
    if size > 65536 {
        file.seek(SeekFrom::Start(size.saturating_sub(65536)))?;
        (&mut *file).take(65536).read_to_end(&mut tail)?;
    }
    Ok((size, prefix, tail))
}
fn fold_metadata(session: &mut Value, record: &Value, id: &str) -> Result<(), Error> {
    if record["sessionId"]
        .as_str()
        .is_some_and(|native| native != id)
    {
        return Err(Error::forbidden("CLAUDE_SESSION_MISMATCH"));
    }
    if session["cwd"].is_null() && record["cwd"].is_string() {
        session["cwd"] = record["cwd"].clone();
    }
    if session["createdAt"].is_null() {
        session["createdAt"] = presentation::time_ms(&record["timestamp"]);
    }
    if record["type"] == "user" && session["title"] == "" {
        session["title"] = json!(text(&record["message"]["content"])
            .chars()
            .take(512)
            .collect::<String>());
    }
    if record["type"] == "custom-title" && record["customTitle"].is_string() {
        session["title"] = record["customTitle"].clone();
    }
    Ok(())
}
fn identity(file: &File) -> Result<(u64, u64), Error> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}
fn lines(bytes: &[u8], start: u64) -> Result<Vec<(u64, Value)>, Error> {
    let mut offset = start;
    let mut records = Vec::new();
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let at = offset;
        offset += line.len() as u64;
        if at == start && start > 0 || line.last() != Some(&b'\n') {
            continue;
        }
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        records.push((
            at,
            serde_json::from_slice(line).map_err(|_| Error::unavailable())?,
        ));
    }
    Ok(records)
}
pub fn resume_path(
    config: &ProfileConfig,
    runtime: Option<&ProjectRuntime>,
    entry: &Entry,
) -> Result<String, Error> {
    let home = Export::open(&config.home)?;
    let (mut file, _) = home.read(&entry.path)?;
    if identity(&file)? != entry.identity {
        return Err(Error::conflict("CLAUDE_HISTORY_REPLACED"));
    }
    if file.metadata()?.len() > MAX_BYTES as u64 {
        return Err(Error::too_large("CLAUDE_RESUME_LIMIT"));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        return Err(Error::too_large("CLAUDE_RESUME_LIMIT"));
    }
    for (_, record) in lines(&bytes, 0)? {
        authorize_record(config, runtime, entry, &record)?;
    }
    config
        .home
        .join(&entry.path)
        .to_str()
        .map(str::to_owned)
        .ok_or_else(Error::invalid)
}
pub fn read(
    config: &ProfileConfig,
    runtime: Option<&ProjectRuntime>,
    entry: &Entry,
    args: &Value,
) -> Result<Value, Error> {
    let home = Export::open(&config.home)?;
    let (mut file, _) = home.read(&entry.path)?;
    if identity(&file)? != entry.identity {
        return Err(Error::conflict("CLAUDE_HISTORY_REPLACED"));
    }
    let end = args["cursor"]
        .as_str()
        .map(|cursor| cursor.parse::<u64>().map_err(|_| Error::invalid()))
        .transpose()?
        .unwrap_or(file.metadata()?.len());
    if end > file.metadata()?.len() {
        return Err(Error::invalid());
    }
    let start = end.saturating_sub(8 * 1024 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    (&mut file).take(end - start).read_to_end(&mut bytes)?;
    let mut records = lines(&bytes, start)?;
    assign_turns(
        &mut records,
        prior_turn(&mut file, start, config, runtime, entry)?,
    );
    page(config, runtime, entry, records, end)
}
fn page(
    config: &ProfileConfig,
    runtime: Option<&ProjectRuntime>,
    entry: &Entry,
    records: Vec<(u64, Value)>,
    end: u64,
) -> Result<Value, Error> {
    let mut turns = Vec::new();
    let (mut total, mut count, mut earliest) = (0, 0, end);
    for (at, record) in records.into_iter().rev() {
        authorize_record(config, runtime, entry, &record)?;
        let items = items(&record);
        if items.as_array().unwrap().is_empty() {
            earliest = at;
            continue;
        }
        let size = items.to_string().len();
        if count + items.as_array().unwrap().len() > 100 || total + size > 2 * 1024 * 1024 {
            break;
        }
        count += items.as_array().unwrap().len();
        total += size;
        earliest = at;
        turns.push(json!({"id":record["_turn"],"items":items}));
    }
    if turns.is_empty() && end > 0 && earliest == end {
        return Err(Error::too_large("CLAUDE_HISTORY_ITEM_LIMIT"));
    }
    turns.reverse();
    Ok(
        json!({"session":entry.session,"turns":turns,"nextCursor":if earliest>0{json!(earliest.to_string())}else{Value::Null}}),
    )
}
pub(super) fn authorize_record(
    config: &ProfileConfig,
    runtime: Option<&ProjectRuntime>,
    entry: &Entry,
    record: &Value,
) -> Result<(), Error> {
    if record["sessionId"]
        .as_str()
        .is_some_and(|id| Some(id) != entry.session["id"].as_str())
        || record["cwd"]
            .as_str()
            .is_some_and(|cwd| !cwd_allowed(config, runtime, cwd))
    {
        return Err(Error::forbidden("CLAUDE_SESSION_MISMATCH"));
    }
    Ok(())
}
pub(super) fn usage_file(config: &ProfileConfig, entry: &Entry) -> Result<File, Error> {
    let (file, _) = Export::open(&config.home)?.read(&entry.path)?;
    if identity(&file)? != entry.identity {
        return Err(Error::conflict("CLAUDE_HISTORY_REPLACED"));
    }
    Ok(file)
}
fn is_user(record: &Value) -> bool {
    record["type"] == "user"
        && record["isMeta"] != true
        && record["isCompactSummary"] != true
        && record["isSidechain"] != true
        && record["message"]["content"]
            .as_array()
            .is_none_or(|parts| !parts.iter().any(|p| p["type"] == "tool_result"))
}
fn assign_turns(records: &mut [(u64, Value)], mut turn: Value) {
    for (_, record) in records {
        if is_user(record) {
            turn = record["uuid"].clone();
        }
        record["_turn"] = turn.clone();
    }
}
fn prior_turn(
    file: &mut File,
    end: u64,
    config: &ProfileConfig,
    runtime: Option<&ProjectRuntime>,
    entry: &Entry,
) -> Result<Value, Error> {
    if end == 0 {
        return Ok(Value::Null);
    }
    let start = end.saturating_sub(1024 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    (&mut *file).take(end - start).read_to_end(&mut bytes)?;
    for (_, record) in lines(&bytes, start)?.into_iter().rev() {
        authorize_record(config, runtime, entry, &record)?;
        if is_user(&record) {
            return Ok(record["uuid"].clone());
        }
    }
    Ok(Value::Null)
}
fn items(record: &Value) -> Value {
    if record["isSidechain"] == true
        || record["isMeta"] == true
        || record["isCompactSummary"] == true
    {
        return json!([]);
    }
    let id = record["uuid"].as_str().unwrap_or("");
    let time = presentation::time_ms(&record["timestamp"]);
    if is_user(record) {
        return user_item(record, id, &time);
    }
    if record["type"] != "assistant" {
        return json!([]);
    }
    let msg = record["message"]["id"].as_str().unwrap_or(id);
    json!(record["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, block)| {
            let mut block = block.clone();
            block["timestamp"] = record["timestamp"].clone();
            super::protocol::item(
                &block,
                &format!("{msg}:{index}"),
                record["_turn"].as_str().unwrap_or(""),
            )
        })
        .collect::<Vec<_>>())
}
fn user_item(record: &Value, id: &str, time: &Value) -> Value {
    let content = if record["message"]["content"].is_string() {
        json!([{"type":"text","text":record["message"]["content"]}])
    } else {
        record["message"]["content"].clone()
    };
    let kinds = content.as_array().map(|parts| parts.iter().map(|part|
        if part["type"] == "text" {json!("user.text")} else {json!("user.attachment")}).collect::<Vec<_>>()).unwrap_or_default();
    json!([{"id":id,"type":"userMessage","content":content,"contentItemKinds":kinds,"turnId":id,"timestamp":time,"status":"completed"}])
}
pub fn text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.into();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
