//! Read-only Linux writer diagnostics. File timestamps never prove lock acquisition time.
use serde_json::{json, Value};
use std::{fs, io::Read, os::unix::fs::MetadataExt, path::Path, time::Instant};

pub(super) fn inspect(home: &Path, id: &str) -> Value {
    let started = Instant::now();
    let mut result =
        observe(home, id).unwrap_or_else(|code| json!({"status":"unknown","code":code}));
    result["checkedAt"] = json!(chrono::Utc::now().timestamp_millis());
    result["acquiredAt"] = Value::Null;
    log_observation(id, &result, started);
    result
}

fn log_observation(id: &str, result: &Value, started: Instant) {
    let mut fields = json!({"sessionId":id});
    fields["operationId"] = json!(format!(
        "writer-lock-{}-{}",
        std::process::id(),
        result["checkedAt"]
    ));
    fields["operation"] = json!("harness.writerLock.inspect");
    fields["stage"] = json!("proc-locks");
    fields["result"] = json!(if result["status"] == "unknown" {
        "failure"
    } else {
        "success"
    });
    fields["lockStatus"] = result["status"].clone();
    fields["owner"] = result["owner"].clone();
    fields["checkedAt"] = result["checkedAt"].clone();
    fields["lockFileModifiedAt"] = result["lockFileModifiedAt"].clone();
    fields["code"] = result["code"].clone();
    fields["durationMs"] = json!(started.elapsed().as_millis() as u64);
    crate::core::events::emit(
        if result["status"] == "unknown" {
            crate::core::events::Level::Warn
        } else {
            crate::core::events::Level::Info
        },
        "harness.writer_lock.observed",
        fields,
    );
}

fn observe(home: &Path, id: &str) -> Result<Value, &'static str> {
    if id.is_empty()
        || id.len() > 128
        || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        return Err("LOCK_ID_INVALID");
    }
    let path = home.join("thread-writer-locks").join(format!("{id}.lock"));
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Err("LOCK_FILE_INVALID"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"status":"not-held"}))
        }
        Err(_) => return Err("LOCK_FILE_UNREADABLE"),
    };
    observe_file(&path, &metadata)
}

fn observe_file(path: &Path, metadata: &fs::Metadata) -> Result<Value, &'static str> {
    let locks =
        read_bounded(Path::new("/proc/locks"), 2 * 1024 * 1024).ok_or("LOCK_TABLE_UNAVAILABLE")?;
    let pid = owner_pid(&locks, metadata)?;
    let current = fs::symlink_metadata(&path).map_err(|_| "LOCK_CHANGED")?;
    if current.ino() != metadata.ino() || current.dev() != metadata.dev() {
        return Err("LOCK_CHANGED");
    }
    let mut result = json!({"status":if pid.is_some() {"held"} else {"not-held"},
        "lockFileModifiedAt":metadata.modified().ok().and_then(|t|t.duration_since(std::time::UNIX_EPOCH).ok()).map(|t|t.as_millis() as u64)});
    if let Some(pid) = pid {
        result["owner"] = process_owner(pid);
        if result["owner"]["uid"].is_null() || result["owner"]["processStartedAt"].is_null() {
            result["code"] = json!("LOCK_OWNER_DETAILS_UNAVAILABLE");
        }
        let latest = read_bounded(Path::new("/proc/locks"), 2 * 1024 * 1024)
            .ok_or("LOCK_TABLE_UNAVAILABLE")?;
        if owner_pid(&latest, metadata)? != Some(pid) {
            return Err("LOCK_CHANGED");
        }
    }
    Ok(result)
}

fn read_bounded(path: &Path, limit: u64) -> Option<String> {
    let mut text = String::new();
    fs::File::open(path)
        .ok()?
        .take(limit + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() as u64 <= limit).then_some(text)
}

fn owner_pid(locks: &str, metadata: &fs::Metadata) -> Result<Option<u32>, &'static str> {
    locks
        .lines()
        .find_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 8
                || fields[1] != "FLOCK"
                || fields[3] != "WRITE"
                || !same_file(fields[5], metadata)
            {
                return None;
            }
            Some(
                fields[4]
                    .parse::<u32>()
                    .ok()
                    .filter(|pid| *pid > 0)
                    .map(Some)
                    .ok_or("LOCK_OWNER_UNAVAILABLE"),
            )
        })
        .unwrap_or(Ok(None))
}

fn same_file(device: &str, metadata: &fs::Metadata) -> bool {
    let mut fields = device.split(':');
    let major = fields
        .next()
        .and_then(|value| u32::from_str_radix(value, 16).ok());
    let minor = fields
        .next()
        .and_then(|value| u32::from_str_radix(value, 16).ok());
    let inode = fields.next().and_then(|value| value.parse::<u64>().ok());
    major == Some(rustix::fs::major(metadata.dev()))
        && minor == Some(rustix::fs::minor(metadata.dev()))
        && inode == Some(metadata.ino())
}

fn process_owner(pid: u32) -> Value {
    let status = read_bounded(Path::new(&format!("/proc/{pid}/status")), 65536).unwrap_or_default();
    let stat = read_bounded(Path::new(&format!("/proc/{pid}/stat")), 65536).unwrap_or_default();
    let uid = status.lines().find_map(|line| {
        line.strip_prefix("Uid:")?
            .split_whitespace()
            .next()?
            .parse::<u32>()
            .ok()
    });
    let boot = read_bounded(Path::new("/proc/stat"), 1024 * 1024).and_then(|text| {
        text.lines()
            .find_map(|line| line.strip_prefix("btime ")?.parse::<u64>().ok())
    });
    json!({"pid":pid,"uid":uid,"processStartedAt":boot.and_then(|boot| process_started_at(&stat, boot, rustix::param::clock_ticks_per_second()))})
}

fn process_started_at(stat: &str, boot: u64, ticks_per_second: u64) -> Option<u64> {
    let ticks = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse::<u64>()
        .ok()?;
    boot.checked_mul(1000)?
        .checked_add(ticks.checked_mul(1000)?.checked_div(ticks_per_second)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn observes_real_flock_owner_without_treating_file_existence_as_a_lock() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join("thread-writer-locks")).unwrap();
        let path = home.path().join("thread-writer-locks/session.lock");
        let file = fs::File::create(&path).unwrap();
        assert_eq!(
            observe(home.path(), "session").unwrap()["status"],
            "not-held"
        );
        rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::LockExclusive).unwrap();
        let held = observe(home.path(), "session").unwrap();
        assert_eq!(held["status"], "held");
        assert_eq!(held["owner"]["pid"], std::process::id());
        assert!(held["owner"]["uid"].is_number());
        assert!(held["owner"]["processStartedAt"].is_number());
        assert!(held.get("acquiredAt").is_none());
        drop(file);
        assert_eq!(
            observe(home.path(), "session").unwrap()["status"],
            "not-held"
        );
        fs::remove_file(path).unwrap();
        assert_eq!(
            observe(home.path(), "session").unwrap()["status"],
            "not-held"
        );
    }

    #[test]
    fn rejects_path_traversal_and_does_not_follow_lock_symlinks() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            observe(home.path(), "../session").unwrap_err(),
            "LOCK_ID_INVALID"
        );
        fs::create_dir(home.path().join("thread-writer-locks")).unwrap();
        std::os::unix::fs::symlink(
            "/proc/locks",
            home.path().join("thread-writer-locks/session.lock"),
        )
        .unwrap();
        assert_eq!(
            observe(home.path(), "session").unwrap_err(),
            "LOCK_FILE_INVALID"
        );
    }

    #[test]
    fn invisible_owner_is_unknown_and_waiters_are_not_owners() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), b"bounded").unwrap();
        let metadata = file.as_file().metadata().unwrap();
        let device = format!(
            "{:02x}:{:02x}:{}",
            rustix::fs::major(metadata.dev()),
            rustix::fs::minor(metadata.dev()),
            metadata.ino()
        );
        let invisible = format!("1: FLOCK ADVISORY WRITE -1 {device} 0 EOF");
        assert_eq!(
            owner_pid(&invisible, &metadata),
            Err("LOCK_OWNER_UNAVAILABLE")
        );
        let waiter = format!("1: -> FLOCK ADVISORY WRITE 42 {device} 0 EOF");
        assert_eq!(owner_pid(&waiter, &metadata), Ok(None));
        assert_eq!(read_bounded(file.path(), 0), None);
    }
}
