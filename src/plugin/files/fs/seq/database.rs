//! Open databases through a validated parent descriptor, never a wire absolute path.
use super::sql;
use crate::{core::error::Error, fs::Export};
use rusqlite::{Connection, OpenFlags};
use rustix::fs::{Mode, OFlags};
use std::{
    fs::File,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    time::Duration,
};

pub(super) fn open(
    export: &Export,
    path: &str,
    write: bool,
) -> Result<Option<(Connection, File, File)>, Error> {
    let (parent, name) = match export.parent(path) {
        Ok(parent) => parent,
        Err(error) if error.code == "ENOENT" => return Ok(None),
        Err(error) => return Err(error),
    };
    let file = match export.open_path(path, if write { OFlags::RDWR } else { OFlags::RDONLY }) {
        Ok(file) => file,
        Err(e) if e.code == "ENOENT" => return Ok(None),
        Err(e) => return Err(e),
    };
    check(&file, write)?;
    // SQLite can open sidecars itself. Reject symlinks and hard links before handing it a path.
    for suffix in ["-journal", "-wal", "-shm"] {
        check_sidecar(&parent, &format!("{name}{suffix}"), write)?;
    }
    let database = Connection::open_with_flags_and_vfs(
        format!("/proc/self/fd/{}/{name}", parent.as_raw_fd()),
        if write {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        },
        super::capability_vfs::name()?,
    )
    .map_err(sql)?;
    // Limit individual values and SQLite allocations, rather than the database file's capacity.
    unsafe {
        rusqlite::ffi::sqlite3_limit(
            database.handle(),
            rusqlite::ffi::SQLITE_LIMIT_LENGTH,
            (super::MAX_BYTES + 4096) as i32,
        );
    }
    database.busy_timeout(Duration::from_secs(5)).map_err(sql)?;
    database
        .execute_batch("PRAGMA trusted_schema=OFF;")
        .map_err(sql)?;
    if write {
        database
            .execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
            .map_err(sql)?;
    }
    // Existing corrupt/foreign files are never silently initialized.
    database
        .prepare("SELECT key,value FROM entries LIMIT 0")
        .map_err(sql)?;
    Ok(Some((database, file, parent)))
}
fn check(file: &File, write: bool) -> Result<(), Error> {
    let info = file.metadata()?;
    if !info.is_file() {
        return Err(Error::unsupported());
    }
    if write
        && (info.nlink() != 1
            || info.uid() != rustix::process::geteuid().as_raw()
            || info.mode() & 0o022 != 0)
    {
        return Err(Error::forbidden("SEQ_PERMISSIONS"));
    }
    Ok(())
}
fn check_sidecar(parent: &File, name: &str, write: bool) -> Result<(), Error> {
    match rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => check(&File::from(fd), write),
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
