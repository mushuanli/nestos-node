//! Directory capabilities.
//!
//! An [`Export`] is an open directory descriptor plus, for writable exports, a
//! revision table. Every lookup is performed relative to that descriptor with
//! `openat2(RESOLVE_BENEATH | NO_SYMLINKS | NO_MAGICLINKS)`, so no wire path
//! can escape the root even if it is a symlink or a mount point.
//!
//! Writable exports additionally hold an advisory `flock` for their lifetime:
//! cooperating instances cannot serve the same directory, which is what makes
//! the in-memory revision table a meaningful conditional-write authority.

use crate::core::error::Error;
use crate::fs::{
    model::{attributes, Entry, Stat},
    path, recovery,
    revision::Revisions,
};
use rustix::fs::{openat2, Mode, OFlags, ResolveFlags};
use std::{
    fs::File,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio_util::sync::CancellationToken;

/// Largest number of children inspected in a single directory scan.
const DIRECTORY_LIMIT: usize = 100_000;

pub struct Export {
    pub(crate) root: File,
    pub(crate) metadata_gate: Mutex<()>,
    revisions: Option<Mutex<Revisions>>,
    pub(crate) upload_journal: Option<Arc<super::upload_journal::UploadJournal>>,
}

impl Export {
    /// Open a read-only capability. Fails closed when `openat2` is unavailable.
    pub fn open(root: &Path) -> Result<Self, Error> {
        let root = File::open(root)?;
        if !root.metadata()?.is_dir() {
            return Err(Error::invalid());
        }
        let export = Self {
            root,
            metadata_gate: Mutex::new(()),
            revisions: None,
            upload_journal: None,
        };
        // Probe the resolution mechanism now rather than at first use.
        export.open_path("", OFlags::PATH)?;
        Ok(export)
    }

    /// Open a writable capability: exclusive lock, revision table, recovery.
    pub fn exclusive(root: &Path) -> Result<Self, Error> {
        let mut export = Self::open(root)?;
        rustix::fs::flock(
            &export.root,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )
        .map_err(|error| {
            if error == rustix::io::Errno::WOULDBLOCK {
                Error::busy()
            } else {
                error.into()
            }
        })?;
        export.revisions = Some(Mutex::new(Revisions::new()?));
        export.upload_journal = Some(Arc::new(super::upload_journal::UploadJournal::open(
            &export.root,
        )?));
        recovery::clean_uploads(&export)?;
        Ok(export)
    }

    /// Whether this export accepts mutations.
    pub fn writable(&self) -> bool {
        self.revisions.is_some()
    }

    /// Duplicate the root descriptor; the duplicate shares the advisory lock.
    pub fn try_clone_root(&self) -> Result<File, Error> {
        Ok(self.root.try_clone()?)
    }

    /// Open a directory inside the export, rejecting symlinks and escapes.
    pub fn open_dir(&self, path: &str) -> Result<File, Error> {
        self.open_path(path, OFlags::RDONLY | OFlags::DIRECTORY)
    }

    /// Look up one entry. `Ok(None)` means "does not exist"; other failures are real.
    pub fn stat(&self, path: &str) -> Result<Option<Stat>, Error> {
        let mut revisions = self.lock_revisions();
        match self.open_path(path, OFlags::PATH) {
            Ok(file) => {
                let mut stat = attributes(&file)?;
                stat.revision = revisions.as_mut().map(|r| r.get(&file)).transpose()?;
                let _metadata = self
                    .metadata_gate
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let (metadata_root, relative) = self.metadata_scope(path)?;
                if let Some(store) = super::metadata::Metadata::open(&metadata_root, false)? {
                    stat.metadata = store.read(&relative)?;
                    stat.source_identity = store.identity(&relative)?;
                }
                Ok(Some(stat))
            }
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Open a regular file for reading together with its current revision.
    pub fn read(&self, path: &str) -> Result<(File, Option<String>), Error> {
        let mut revisions = self.lock_revisions();
        let file = self.open_path(path, OFlags::RDONLY)?;
        if !file.metadata()?.is_file() {
            return Err(Error::unsupported());
        }
        let revision = revisions.as_mut().map(|r| r.get(&file)).transpose()?;
        if path.ends_with(".seq") {
            use std::os::unix::fs::FileExt;
            let mut header = [0u8; 16];
            if file.read_at(&mut header, 0)? == 16 && &header == b"SQLite format 3\0" {
                return Ok((super::seq::backup_file(self, path)?, revision));
            }
        }
        Ok((file, revision))
    }

    /// Sorted children of `path` plus diagnostics for skipped entries.
    pub fn list(&self, path: &str, cancelled: &CancellationToken) -> Result<Listing, Error> {
        let directory = self.open_dir(path)?;
        let mut listing = Listing::default();
        for item in rustix::fs::Dir::read_from(&directory)? {
            if cancelled.is_cancelled() {
                return Err(Error::cancelled());
            }
            let item = item?;
            let Ok(name) = item.file_name().to_str() else {
                listing.warnings.push("NON_UTF8_NAME");
                continue;
            };
            if name == "." || name == ".." || path::is_reserved(name) {
                continue;
            }
            if listing.entries.len() + listing.warnings.len() >= DIRECTORY_LIMIT {
                return Err(Error::too_large("DIRECTORY_LIMIT"));
            }
            let child = child_path(path, name);
            match self.stat(&child) {
                Ok(Some(stat)) => listing.entries.push(Entry {
                    name: name.into(),
                    stat,
                }),
                Ok(None) => {}
                Err(error) if error.is_unsupported() => listing.warnings.push("UNSUPPORTED_NODE"),
                Err(error) => return Err(error),
            }
        }
        listing.entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(listing)
    }

    /// Drop every remembered revision, e.g. after a command may have written.
    pub fn invalidate_revisions(&self) {
        if let Some(mut revisions) = self.lock_revisions() {
            revisions.invalidate();
        }
    }

    /// Borrow the revision table of a writable export for one atomic step.
    ///
    /// A poisoned lock is recovered rather than propagated: [`Revisions`] only
    /// ever holds a counter and a map, so a panic elsewhere cannot leave it in
    /// a half-updated state that matters.
    pub(crate) fn lock_revisions(&self) -> Option<MutexGuard<'_, Revisions>> {
        self.revisions
            .as_ref()
            .map(|lock| lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    /// Resolve `path` under the root capability.
    pub(crate) fn open_path(&self, path: &str, flags: OFlags) -> Result<File, Error> {
        path::validate(path)?;
        let flags = flags | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        // PATH lookups are metadata probes; everything else must not block.
        let flags = if flags.contains(OFlags::PATH) {
            flags
        } else {
            flags | OFlags::NONBLOCK
        };
        let fd = openat2(
            &self.root,
            if path.is_empty() { "." } else { path },
            flags,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        )?;
        Ok(File::from(fd))
    }

    /// Split a non-root path into its parent directory and final component.
    pub(crate) fn parent(&self, path: &str) -> Result<(File, String), Error> {
        path::validate(path)?;
        if path.is_empty() {
            return Err(Error::invalid());
        }
        let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
        Ok((self.open_dir(directory)?, name.to_owned()))
    }
}

/// Result of one directory scan.
#[derive(Default)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub warnings: Vec<&'static str>,
}

pub(crate) fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}
