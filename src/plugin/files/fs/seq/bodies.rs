//! Immutable, content-addressed JSON bodies, published before referencing SeqFile rows.
use super::validate;
use crate::{
    core::error::Error,
    fs::{upload, Export},
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const MAX_BODY: usize = 1024 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BodyQuery {
    pub path: String,
    pub sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BodyWrite {
    pub path: String,
    pub sha256: String,
    pub text: String,
}
pub fn body_path(path: &str, hash: &str) -> Result<String, Error> {
    validate(path)?;
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::invalid());
    }
    Ok(format!("{path}.assets/{hash}.json"))
}
pub fn read(export: &Export, query: &BodyQuery) -> Result<Value, Error> {
    let path = body_path(&query.path, &query.sha256)?;
    let (file, _) = match export.read(&path) {
        Ok(found) => found,
        Err(e) if e.code == "ENOENT" => return Ok(json!({"text": null})),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.take((MAX_BODY + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BODY {
        return Err(Error::too_large("SEQ_BODY_LIMIT"));
    }
    if format!("{:x}", Sha256::digest(&bytes)) != query.sha256 {
        return Err(Error::conflict("SEQ_BODY_INTEGRITY"));
    }
    let text = String::from_utf8(bytes).map_err(|_| Error::conflict("SEQ_BODY_INTEGRITY"))?;
    Ok(json!({"text": text}))
}
pub fn write(
    export: &Export,
    input: BodyWrite,
    checkpoint: impl Fn() -> Result<(), Error>,
) -> Result<Value, Error> {
    if !export.writable() {
        return Err(Error::forbidden("EROFS"));
    }
    checkpoint()?;
    let path = body_path(&input.path, &input.sha256)?;
    if input.text.len() > MAX_BODY {
        return Err(Error::too_large("SEQ_BODY_LIMIT"));
    }
    if format!("{:x}", Sha256::digest(input.text.as_bytes())) != input.sha256 {
        return Err(Error::invalid());
    }
    let query = BodyQuery {
        path: input.path.clone(),
        sha256: input.sha256.clone(),
    };
    if read(export, &query)?["text"].is_string() {
        return Ok(json!({"sha256": input.sha256}));
    }
    let directory = format!("{}.assets", input.path);
    ensure_directory(export, &directory, &checkpoint)?;
    let staged = export.stage(&path)?;
    staged.writer()?.write_all(input.text.as_bytes())?;
    match upload::commit(export, &path, staged, None, &checkpoint) {
        Ok(_) => (),
        Err(e) if e.code == "ECONFLICT" => {
            if !read(export, &query)?["text"].is_string() {
                return Err(e);
            }
        }
        Err(e) => return Err(e),
    }
    Ok(json!({"sha256": input.sha256}))
}

fn ensure_directory(
    export: &Export,
    path: &str,
    checkpoint: &impl Fn() -> Result<(), Error>,
) -> Result<(), Error> {
    let _revisions = export
        .lock_revisions()
        .ok_or_else(|| Error::forbidden("EROFS"))?;
    checkpoint()?;
    let (parent, name) = export.parent(path)?;
    match rustix::fs::mkdirat(
        &parent,
        name.as_str(),
        rustix::fs::Mode::from_bits_truncate(0o700),
    ) {
        Ok(()) => parent.sync_all()?,
        Err(rustix::io::Errno::EXIST) => {
            export.open_dir(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    // Creating a new sibling directory does not change the database inode or its validator.
    Ok(())
}
