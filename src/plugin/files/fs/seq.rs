//! Structured SQLite SeqFiles. Existing databases use native, page-level transactions.
use crate::{
    core::error::Error,
    fs::{upload, Export},
};
use rusqlite::{Connection, DatabaseName, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;

pub mod bodies;
mod capability_vfs;
mod database;
const MAX_BYTES: usize = 16 * 1024 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Update {
    pub path: String,
    pub expected_revision: Option<String>,
    pub changes: Vec<Change>,
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub enum Change {
    Set { key: String, value: Value },
    Delete { key: String },
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub revision: Option<String>,
    pub entries: Vec<Entry>,
}
#[derive(Serialize)]
pub struct Entry {
    pub key: String,
    pub value: Value,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Selection {
    pub keys: Option<Vec<String>>,
    pub prefix: Option<String>,
    pub after: Option<String>,
    pub limit: Option<usize>,
    pub expected_revision: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    #[serde(flatten)]
    pub snapshot: Snapshot,
    pub next: Option<String>,
}

pub fn snapshot(export: &Export, path: &str) -> Result<Snapshot, Error> {
    // Compatibility endpoint remains bounded; interactive clients use query.
    let page = query(
        export,
        path,
        Selection {
            limit: Some(1000),
            ..Selection::default()
        },
    )?;
    if page.next.is_some() {
        return Err(Error::too_large("SEQ_RESPONSE_LIMIT"));
    }
    Ok(page.snapshot)
}
pub fn query(export: &Export, path: &str, input: Selection) -> Result<Page, Error> {
    validate(path)?;
    validate_selection(&input)?;
    let mut revisions = export.lock_revisions();
    let opened = database::open(export, path, false)?;
    let Some((db, file, _parent)) = opened else {
        if input.expected_revision.is_some() {
            return Err(Error::precondition_failed());
        }
        return Ok(Page {
            snapshot: Snapshot {
                revision: None,
                entries: vec![],
            },
            next: None,
        });
    };
    let revision = revisions.as_mut().map(|r| r.get(&file)).transpose()?;
    if input.expected_revision.is_some() && input.expected_revision != revision {
        return Err(Error::precondition_failed());
    }
    db.execute_batch("BEGIN DEFERRED;").map_err(sql)?;
    let entries = select(&db, &input)?;
    let limit = input.limit.unwrap_or(100);
    let next = if input.keys.is_none() && entries.len() > limit {
        entries.get(limit - 1).map(|e| e.key.clone())
    } else {
        None
    };
    Ok(Page {
        snapshot: Snapshot {
            revision,
            entries: entries
                .into_iter()
                .take(if input.keys.is_some() { 256 } else { limit })
                .collect(),
        },
        next,
    })
}
fn validate_selection(input: &Selection) -> Result<(), Error> {
    if !(1..=1000).contains(&input.limit.unwrap_or(100)) {
        return Err(Error::invalid());
    }
    if let Some(keys) = &input.keys {
        if keys.len() > 256
            || input.prefix.is_some()
            || input.after.is_some()
            || input.limit.is_some()
        {
            return Err(Error::invalid());
        }
        for key in keys {
            validate_key(key)?;
        }
    }
    if let Some(prefix) = &input.prefix {
        if !prefix.is_empty() {
            validate_key(prefix)?;
        }
    }
    if let Some(after) = &input.after {
        validate_key(after)?;
    }
    Ok(())
}
fn select(db: &Connection, input: &Selection) -> Result<Vec<Entry>, Error> {
    let mut entries = Vec::new();
    let mut bytes = 0usize;
    if let Some(keys) = &input.keys {
        let mut statement = db
            .prepare("SELECT value FROM entries WHERE key=?1")
            .map_err(sql)?;
        let mut seen = std::collections::HashSet::new();
        for key in keys {
            if !seen.insert(key) {
                continue;
            }
            let value: Option<String> = statement
                .query_row([key], |r| r.get(0))
                .optional()
                .map_err(sql)?;
            if let Some(value) = value {
                push_entry(&mut entries, key.clone(), value, &mut bytes)?;
            }
        }
    } else {
        let prefix = input.prefix.as_deref().unwrap_or("");
        // A lexical range seeks the primary-key index; LIKE would mishandle % and _.
        let upper = prefix_upper(prefix);
        let after = input.after.as_deref().unwrap_or("");
        let lower = prefix.max(after);
        let end = if upper.is_some() {
            "key<?2"
        } else {
            "?2 IS NULL"
        };
        let query = format!("SELECT key,value FROM entries WHERE key>=?1 AND {end} AND key!=?3 ORDER BY key LIMIT ?4");
        let mut statement = db.prepare(&query).map_err(sql)?;
        let rows = statement
            .query_map(
                rusqlite::params![lower, upper, after, input.limit.unwrap_or(100) + 1],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .map_err(sql)?;
        for row in rows {
            let (key, value) = row.map_err(sql)?;
            push_entry(&mut entries, key, value, &mut bytes)?;
        }
    }
    Ok(entries)
}
fn prefix_upper(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        let next = (last as u32 + 1..=0x10ffff).find_map(char::from_u32);
        if let Some(next) = next {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
    }
    None
}
fn push_entry(
    entries: &mut Vec<Entry>,
    key: String,
    value: String,
    bytes: &mut usize,
) -> Result<(), Error> {
    *bytes += key.len() + value.len();
    if *bytes > MAX_BYTES {
        return Err(Error::too_large("SEQ_RESPONSE_LIMIT"));
    }
    entries.push(Entry {
        key,
        value: serde_json::from_str(&value).map_err(|_| Error::internal())?,
    });
    Ok(())
}
pub fn update(
    export: &Export,
    input: Update,
    checkpoint: impl Fn() -> Result<(), Error>,
) -> Result<Value, Error> {
    if !export.writable() {
        return Err(Error::forbidden("EROFS"));
    }
    validate(&input.path)?;
    if input.changes.len() > 256 {
        return Err(Error::invalid());
    }
    checkpoint()?;
    // Existing files commit in place under the same revision lock as uploads/renames.
    let mut revisions = export
        .lock_revisions()
        .ok_or_else(|| Error::forbidden("EROFS"))?;
    if let Some((mut db, file, _parent)) = database::open(export, &input.path, true)? {
        let revision = revisions.get(&file)?;
        if Some(&revision) != input.expected_revision.as_ref() {
            return Err(Error::precondition_failed());
        }
        // Invalidate before commit, including uncertain I/O failures. Never reuse an old validator.
        revisions.retire(&file)?;
        let revision = revisions.get(&file)?;
        apply(&mut db, input.changes, &checkpoint)?;
        return Ok(serde_json::json!({"revision": revision}));
    }
    if input.expected_revision.is_some() {
        return Err(Error::precondition_failed());
    }
    drop(revisions);
    // Only first creation publishes a small standalone SQLite image atomically.
    let mut db = Connection::open_in_memory().map_err(sql)?;
    db.execute_batch(
        "CREATE TABLE entries(key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL) WITHOUT ROWID;",
    )
    .map_err(sql)?;
    apply(&mut db, input.changes, &checkpoint)?;
    publish(export, &input.path, &db, None, checkpoint)
}
fn publish(
    export: &Export,
    path: &str,
    db: &Connection,
    revision: Option<&str>,
    checkpoint: impl Fn() -> Result<(), Error>,
) -> Result<Value, Error> {
    let bytes = db.serialize(DatabaseName::Main).map_err(sql)?;
    if bytes.len() > MAX_BYTES {
        return Err(Error::invalid());
    }
    let staged = export.stage(path)?;
    staged.writer()?.write_all(&bytes)?;
    serde_json::to_value(upload::commit(export, path, staged, revision, checkpoint)?)
        .map_err(|_| Error::internal())
}
fn apply(
    db: &mut Connection,
    changes: Vec<Change>,
    checkpoint: &impl Fn() -> Result<(), Error>,
) -> Result<(), Error> {
    let tx = db.transaction().map_err(sql)?;
    for change in changes {
        checkpoint()?;
        match change {
            Change::Set { key, value } => {
                validate_key(&key)?;
                tx.execute("INSERT INTO entries(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", (&key, value.to_string())).map_err(sql)?;
            }
            Change::Delete { key } => {
                validate_key(&key)?;
                tx.execute("DELETE FROM entries WHERE key=?1", [&key])
                    .map_err(sql)?;
            }
        }
    }
    checkpoint()?;
    tx.commit().map_err(|failure| {
        let mut error = sql(failure);
        error.outcome = Some("unknown");
        error
    })
}
fn validate(path: &str) -> Result<(), Error> {
    crate::fs::path::validate(path)?;
    if !path.ends_with(".seq") {
        return Err(Error::invalid());
    }
    Ok(())
}
fn validate_key(key: &str) -> Result<(), Error> {
    if key.is_empty() || key.len() > 1024 || key.contains('\0') {
        return Err(Error::invalid());
    }
    Ok(())
}
fn sql(error: rusqlite::Error) -> Error {
    if let rusqlite::Error::SqliteFailure(failure, _) = error {
        use rusqlite::ErrorCode;
        return match failure.code {
            ErrorCode::TooBig => Error::too_large("SEQ_RESPONSE_LIMIT"),
            ErrorCode::DiskFull => {
                Error::new(axum::http::StatusCode::INSUFFICIENT_STORAGE, "ENOSPC")
            }
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => Error::busy(),
            _ => Error::internal(),
        };
    }
    Error::internal()
}

/// A raw file download requests a stable backup, never the live mutable database inode.
pub(crate) fn backup_file(export: &Export, path: &str) -> Result<std::fs::File, Error> {
    let (db, _file, _parent) =
        database::open(export, path, false)?.ok_or_else(|| Error::not_found("ENOENT"))?;
    let directory = tempfile::tempdir()?;
    let target = directory.path().join("snapshot.db");
    db.backup(DatabaseName::Main, &target, None).map_err(sql)?;
    let file = std::fs::File::open(target)?;
    // The private temporary directory is unlinked; the returned descriptor owns the snapshot.
    Ok(file)
}
