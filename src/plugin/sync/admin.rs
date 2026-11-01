//! Offline administration shares the service lock and never edits an open root.
use super::{
    model::*,
    policy,
    store::{blobs, metadata as m},
    SyncService,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn run(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        return Err(Error::new(
            "usage: cells sync <init|verify|gc|backup|restore|repair> CONFIG [arguments]",
            400,
        ));
    }
    let config = crate::config::launch::load(Path::new(&args[1]))
        .map_err(|_| Error::new("INVALID_CONFIG", 400))?;
    let c = config
        .sync
        .as_ref()
        .filter(|s| s.enabled)
        .ok_or_else(|| Error::new("SYNC_DISABLED", 400))?;
    let result = match args[0].as_str() {
        "init" if args.len() == 2 => {
            SyncService::init(c)?;
            json!({"initialized":true})
        }
        "restore" if args.len() == 3 => restore(c, Path::new(&args[2]))?,
        "verify" if args.len() == 2 => SyncService::open_verify(c)?.verify()?,
        "gc" if args.len() == 2 => SyncService::open(c)?.gc()?,
        "backup" if args.len() == 3 => SyncService::open(c)?.backup(Path::new(&args[2]))?,
        "repair" if args.len() == 5 => {
            SyncService::open(c)?.repair(&args[2], &args[3], Path::new(&args[4]))?
        }
        _ => return Err(Error::new("INVALID_ADMIN_ARGUMENTS", 400)),
    };
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
impl SyncService {
    pub fn verify(&self) -> Result<Value> {
        let db = self.connection()?;
        if !db.is_autocommit() {
            return Err(Error::storage());
        }
        verify_database(&db)?;
        let count = self.verify_objects(&db)?;
        self.verify_history(&db)?;
        for (_, p) in m::list::<Project>(&db, "", "project")? {
            self.verify_pins(&db, &p.project_id)?;
            for (_, d) in m::list::<Dataset>(&db, &p.project_id, "dataset")? {
                if (p.state == "active" || p.recoverable_until.unwrap_or(0) > self.time())
                    && d.state == "active"
                {
                    self.verify_manifest(&db, &p.project_id, &d.head.manifest_hash, &d.kind)?;
                }
            }
        }
        Ok(
            json!({"verifiedObjects":count,"authorityId":self.identity.authority_id,"historyEpoch":self.epoch()}),
        )
    }
    fn verify_manifest(
        &self,
        db: &rusqlite::Connection,
        project: &str,
        hash: &str,
        kind: &str,
    ) -> Result<()> {
        let parsed = self.validate_manifest(db, project, hash, kind)?;
        let cached: Value = m::get(db, project, "manifest", hash)?
            .ok_or_else(|| Error::new("REFERENCE_INTEGRITY_FAILED", 503))?;
        let expected: std::collections::BTreeSet<_> =
            parsed.refs.into_iter().map(|r| r.hash).collect();
        let mut stmt =
            db.prepare("SELECT hash FROM manifest_refs WHERE project=?1 AND manifest=?2")?;
        let actual = stmt
            .query_map((project, hash), |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()?;
        if expected != actual || cached["format"] != parsed.format {
            return Err(Error::new("REFERENCE_INTEGRITY_FAILED", 503));
        }
        Ok(())
    }
    fn verify_pins(&self, db: &rusqlite::Connection, project: &str) -> Result<()> {
        for (_, pin) in m::list::<Value>(db, project, "pin")? {
            if pin["expires"].as_u64().unwrap_or(0) <= self.time() {
                continue;
            }
            let hash = pin["manifestHash"].as_str().ok_or_else(Error::storage)?;
            let cached: Value = m::require(db, project, "manifest", hash)?;
            let kind = if cached["format"] == "fs-agent.files" {
                "files"
            } else {
                "bundle"
            };
            self.verify_manifest(db, project, hash, kind)?;
        }
        Ok(())
    }
    fn verify_history(&self, db: &rusqlite::Connection) -> Result<()> {
        let mut stmt=db.prepare("SELECT scope,key,value FROM records WHERE kind='version' AND json_extract(value,'$.retainUntil')>?1")?;
        let rows = stmt.query_map([self.time()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (project, key, raw) = row?;
            let id = key.split('/').next().ok_or_else(Error::storage)?;
            let dataset: Dataset = m::require(db, &project, "dataset", id)?;
            let version: Version = serde_json::from_str(&raw).map_err(|_| Error::storage())?;
            self.verify_manifest(db, &project, &version.head.manifest_hash, &dataset.kind)?;
        }
        Ok(())
    }
    fn verify_objects(&self, db: &rusqlite::Connection) -> Result<u64> {
        let mut stmt = db.prepare("SELECT project,hash,size,state FROM objects")?;
        let mut count = 0;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (project, hash, size, state) = row?;
            if state != "ready" || blobs::verify(&self.path(&project, &hash)?, &hash)? != size {
                return Err(Error::new("OBJECT_CORRUPT", 503));
            }
            count += 1;
        }
        Ok(count)
    }
    pub fn backup(&self, destination: &Path) -> Result<Value> {
        self.stop();
        self.drain()?;
        self.verify()?;
        let db = self.connection()?;
        require_empty(destination)?;
        reject_nested(&self.config.root, destination)?;
        blobs::private_dir(destination)?;
        let _destination_lock = blobs::lock(destination)?;
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        copy_tree(&self.config.root, destination, true)?;
        let files = file_manifest(destination)?;
        let backup = json!({"backupId":random_id()?,"formatVersion":1,"schemaVersion":1,
            "authorityId":self.identity.authority_id,"historyEpoch":self.epoch(),"createdAt":self.time(),"files":files});
        let bytes = serde_json::to_vec(&backup)?;
        blobs::atomic(&destination.join("backup.json"), &bytes)?;
        verify_files(destination, &backup)?;
        // Validate a separate copy so SQLite never mutates the final backup.
        let validation = destination.with_extension(format!("verify-{}", random_id()?));
        blobs::private_dir(&validation)?;
        copy_tree(destination, &validation, true)?;
        let mut config = self.config.clone();
        config.root = validation.clone();
        let verified = SyncService::open(&config).and_then(|s| s.verify());
        std::fs::remove_dir_all(&validation)?;
        verified?;
        blobs::atomic(&destination.join("COMPLETE"), digest(&bytes).as_bytes())?;
        Ok(json!({"backupId":backup["backupId"],"complete":true}))
    }
    pub fn repair(&self, project: &str, hash: &str, source: &Path) -> Result<Value> {
        policy::id(project)?;
        policy::hash(hash)?;
        let size = blobs::verify(source, hash)?;
        let target = self.path(project, hash)?;
        let temporary = target.with_extension("repair");
        let db = self.connection()?;
        self.project(&db, project, false)?;
        let expected: u64 = db.query_row(
            "SELECT size FROM objects WHERE project=?1 AND hash=?2",
            (project, hash),
            |r| r.get(0),
        )?;
        if size != expected {
            return Err(Error::new("OBJECT_SIZE_MISMATCH", 422));
        }
        blobs::space(&self.config.root, self.config.metadata_reserve_bytes, size)?;
        durable_copy(source, &temporary)?;
        if blobs::verify(&temporary, hash)? != size {
            return Err(Error::new("OBJECT_CORRUPT", 503));
        }
        let intent = json!({"project":project,"hash":hash,"size":size,"startedAt":self.time()});
        m::put(&db, "", "repair", &format!("{project}/{hash}"), &intent)?;
        super::fault::point("after-repair-intent");
        self.finish_repair(&db, project, hash)?;
        Ok(json!({"repaired":hash}))
    }
    pub(super) fn recover_repairs(&self, db: &rusqlite::Connection) -> Result<()> {
        for (_, intent) in m::list::<Value>(db, "", "repair")? {
            self.finish_repair(
                db,
                intent["project"].as_str().ok_or_else(Error::storage)?,
                intent["hash"].as_str().ok_or_else(Error::storage)?,
            )?;
        }
        Ok(())
    }
    fn finish_repair(&self, db: &rusqlite::Connection, project: &str, hash: &str) -> Result<()> {
        let target = self.path(project, hash)?;
        let temporary = target.with_extension("repair");
        if temporary.exists() {
            blobs::verify(&temporary, hash)?;
            if target.exists() {
                let damaged = target.with_extension(format!("damaged-{}", random_id()?));
                std::fs::rename(&target, damaged)?;
                blobs::sync_dir(target.parent().unwrap())?;
            }
            std::fs::rename(&temporary, &target)?;
            blobs::sync_dir(target.parent().unwrap())?;
        }
        blobs::verify(&target, hash)?;
        db.execute(
            "UPDATE objects SET state='ready' WHERE project=?1 AND hash=?2",
            (project, hash),
        )?;
        m::put(
            db,
            "",
            "repair-audit",
            &random_id()?,
            &json!({"project":project,"hash":hash,"finishedAt":self.time()}),
        )?;
        m::remove(db, "", "repair", &format!("{project}/{hash}"))
    }
}
fn require_empty(path: &Path) -> Result<()> {
    if path.exists() && std::fs::read_dir(path)?.next().is_some() {
        return Err(Error::new("DESTINATION_NOT_EMPTY", 409));
    }
    Ok(())
}
fn reject_nested(source: &Path, dest: &Path) -> Result<()> {
    let source = std::fs::canonicalize(source)?;
    let dest = if dest.exists() {
        std::fs::canonicalize(dest)?
    } else {
        std::fs::canonicalize(dest.parent().ok_or_else(Error::storage)?)?
            .join(dest.file_name().ok_or_else(Error::storage)?)
    };
    if super::store::boundary::overlap(&source, &dest) {
        return Err(Error::new("BACKUP_ROOT_OVERLAP", 400));
    }
    Ok(())
}
fn durable_copy(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        blobs::private_dir(parent)?;
    }
    std::fs::copy(source, dest)?;
    std::fs::File::open(dest)?.sync_all()?;
    blobs::sync_dir(dest.parent().unwrap())
}
fn copy_tree(source: &Path, dest: &Path, skip_admin: bool) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let kind = entry.file_type()?;
        if name == "sync.lock"
            || name == "metadata.db-shm"
            || (skip_admin && (name == "backup.json" || name == "COMPLETE"))
        {
            continue;
        }
        let target = dest.join(name);
        if kind.is_dir() {
            blobs::private_dir(&target)?;
            copy_tree(&entry.path(), &target, false)?;
        } else if kind.is_file() {
            durable_copy(&entry.path(), &target)?;
        } else {
            return Err(Error::new("UNSAFE_BACKUP_FILE", 400));
        }
    }
    blobs::sync_dir(dest)
}
fn file_manifest(root: &Path) -> Result<Vec<Value>> {
    let mut paths = Vec::new();
    enumerate(root, root, &mut paths)?;
    let mut files = Vec::new();
    for path in paths {
        let relative = path.strip_prefix(root).map_err(|_| Error::storage())?;
        if relative == Path::new("sync.lock")
            || relative == Path::new("backup.json")
            || relative == Path::new("COMPLETE")
            || relative == Path::new("restore.pending")
        {
            continue;
        }
        let (hash, size) = blobs::hash_file(&path)?;
        files.push(
            json!({"path":relative.to_str().ok_or_else(Error::storage)?,"hash":hash,"size":size}),
        );
    }
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    Ok(files)
}
fn enumerate(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            enumerate(root, &entry.path(), files)?;
        } else if entry.file_type()?.is_file() {
            files.push(entry.path());
        } else {
            return Err(Error::new("UNSAFE_BACKUP_FILE", 400));
        }
    }
    let _ = root;
    Ok(())
}
fn verify_files(root: &Path, backup: &Value) -> Result<()> {
    if backup["formatVersion"] != 1 || backup["schemaVersion"] != 1 {
        return Err(Error::new("INVALID_BACKUP", 400));
    }
    let actual = file_manifest(root)?;
    if backup["files"] != json!(actual) {
        return Err(Error::new("BACKUP_CORRUPT", 503));
    }
    Ok(())
}
fn restore(config: &Config, backup: &Path) -> Result<Value> {
    policy::validate(config)?;
    reject_nested(backup, &config.root)?;
    let lock = prepare_restore(config, backup)?;
    let identity = restore_identity(config)?;
    blobs::atomic(
        &config.root.join("storage.json"),
        &serde_json::to_vec(&identity)?,
    )?;
    drop(lock);
    let restored = SyncService::open_restoring(config)?;
    restored.verify()?;
    std::fs::remove_file(config.root.join("restore.pending"))?;
    blobs::sync_dir(&config.root)?;
    Ok(
        json!({"restored":true,"authorityId":identity.authority_id,"historyEpoch":identity.history_epoch}),
    )
}
fn prepare_restore(config: &Config, backup: &Path) -> Result<std::fs::File> {
    let bytes = std::fs::read(backup.join("backup.json"))?;
    let hash = digest(&bytes);
    if std::fs::read_to_string(backup.join("COMPLETE"))? != hash {
        return Err(Error::new("BACKUP_INCOMPLETE", 400));
    }
    let record: Value = serde_json::from_slice(&bytes)?;
    verify_files(backup, &record)?;
    let pending = config.root.join("restore.pending");
    if pending.exists() {
        if std::fs::read_to_string(&pending)? != hash {
            return Err(Error::new("RESTORE_SOURCE_CHANGED", 409));
        }
    } else {
        require_empty(&config.root)?;
    }
    blobs::private_dir(&config.root)?;
    let lock = blobs::lock(&config.root)?;
    blobs::atomic(&pending, hash.as_bytes())?;
    reset_database_files(&config.root)?;
    copy_tree(backup, &config.root, true)?;
    verify_files(&config.root, &record)?;
    super::fault::point("after-restore-copy");
    Ok(lock)
}
fn reset_database_files(root: &Path) -> Result<()> {
    for file in ["metadata.db", "metadata.db-wal", "metadata.db-shm"] {
        let path = root.join(file);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    blobs::sync_dir(root)
}
fn restore_identity(config: &Config) -> Result<Identity> {
    let db = m::open(&config.root.join("metadata.db"))?;
    let mut identity: Identity = m::require(&db, "", "info", "identity")?;
    if identity.principal_id != config.principal_id
        || identity.namespace_id != config.namespace_id
        || config
            .expected_authority_id
            .as_ref()
            .is_some_and(|id| id != &identity.authority_id)
    {
        return Err(Error::new("SYNC_IDENTITY_MISMATCH", 400));
    }
    identity.history_epoch = random_id()?;
    identity.cursor_key = random_id()?;
    m::put(&db, "", "info", "identity", &identity)?;
    db.execute("DELETE FROM records WHERE kind IN ('pin','ack')", [])?;
    db.execute("DELETE FROM uploads", [])?;
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    Ok(identity)
}

fn verify_database(db: &rusqlite::Connection) -> Result<()> {
    let result: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    let foreign_keys: u64 =
        db.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    if result != "ok" || foreign_keys != 0 {
        return Err(Error::new("DATABASE_CORRUPT", 503));
    }
    Ok(())
}
