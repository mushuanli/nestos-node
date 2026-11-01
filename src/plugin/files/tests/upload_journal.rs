//! Crash recovery follows durable staging records, never unrelated export contents.
use cells::fs::{upload, Export};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

const STATE: &str = ".itookit-upload-state/uploads.sqlite";

// Fork/exec briefly inherits other threads' open flock descriptors before CLOEXEC runs.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

fn count(root: &Path) -> i64 {
    let db = rusqlite::Connection::open(root.join(STATE)).unwrap();
    db.query_row("SELECT count(*) FROM uploads", [], |row| row.get(0))
        .unwrap()
}

fn staged_name(root: &Path) -> PathBuf {
    fs::read_dir(root.join("nested"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(".itookit-upload-")
        })
        .unwrap()
}

fn crash(root: &Path, published: bool) {
    fs::create_dir(root.join("nested")).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "upload_crash_worker", "--nocapture"])
        .env("CELLS_UPLOAD_TEST_ROOT", root)
        .env(
            "CELLS_UPLOAD_TEST_PUBLISHED",
            if published { "1" } else { "0" },
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(count(root), 1);
}

#[test]
fn upload_crash_worker() {
    let _guard = serial();
    let Some(root) = std::env::var_os("CELLS_UPLOAD_TEST_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let export = Export::exclusive(root).unwrap();
    let upload = export.stage("nested/result").unwrap();
    let mut writer = upload.writer().unwrap();
    writer.write_all(b"complete body").unwrap();
    writer.sync_all().unwrap();
    if std::env::var("CELLS_UPLOAD_TEST_PUBLISHED").unwrap() == "1" {
        fs::rename(staged_name(root), root.join("nested/result")).unwrap();
        fs::File::open(root.join("nested"))
            .unwrap()
            .sync_all()
            .unwrap();
    }
    // Exit without destructors, as a killed process would; SQLite transactions already committed.
    std::process::exit(0);
}

#[test]
fn a_crashed_upload_is_removed_from_its_recorded_directory() {
    let _guard = serial();
    let root = tempfile::tempdir().unwrap();
    crash(root.path(), false);
    let name = staged_name(root.path());
    let _export = Export::exclusive(root.path()).unwrap();
    assert!(!name.exists());
    assert_eq!(count(root.path()), 0);
}

#[test]
fn recovery_after_publish_only_retires_the_record_and_preserves_final_bytes() {
    let _guard = serial();
    let root = tempfile::tempdir().unwrap();
    crash(root.path(), true);
    let _export = Export::exclusive(root.path()).unwrap();
    assert_eq!(
        fs::read(root.path().join("nested/result")).unwrap(),
        b"complete body"
    );
    assert_eq!(count(root.path()), 0);
}

#[test]
fn cancellation_and_commit_retire_their_records_and_keep_the_store_private() {
    let _guard = serial();
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    let staged = export.stage("cancelled").unwrap();
    assert_eq!(count(root.path()), 1);
    drop(staged);
    assert_eq!(count(root.path()), 0);
    let staged = export.stage("result").unwrap();
    staged.writer().unwrap().write_all(b"body").unwrap();
    upload::commit(&export, "result", staged, None, || Ok(())).unwrap();
    assert_eq!(count(root.path()), 0);
    assert_eq!(fs::read(root.path().join("result")).unwrap(), b"body");
    assert_eq!(
        fs::metadata(root.path().join(".itookit-upload-state"))
            .unwrap()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(root.path().join(STATE)).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(
        export.list("", &Default::default()).unwrap().entries.len(),
        1
    );
}

#[test]
fn recovery_keeps_a_replacement_file_and_its_unresolved_record() {
    let _guard = serial();
    let root = tempfile::tempdir().unwrap();
    crash(root.path(), false);
    let name = staged_name(root.path());
    fs::write(root.path().join("replacement"), b"unrelated").unwrap();
    fs::rename(root.path().join("replacement"), &name).unwrap();
    let _export = Export::exclusive(root.path()).unwrap();
    assert_eq!(fs::read(name).unwrap(), b"unrelated");
    assert_eq!(count(root.path()), 1);
}

#[test]
fn an_unverified_create_intent_cannot_delete_an_existing_file() {
    let _guard = serial();
    let root = tempfile::tempdir().unwrap();
    crash(root.path(), false);
    let name = staged_name(root.path());
    rusqlite::Connection::open(root.path().join(STATE))
        .unwrap()
        .execute("UPDATE uploads SET device=NULL,inode=NULL", [])
        .unwrap();
    let _export = Export::exclusive(root.path()).unwrap();
    assert!(name.exists());
    assert_eq!(count(root.path()), 1);
}

#[test]
fn replacing_a_parent_or_a_staged_file_with_a_symlink_cannot_escape_the_export() {
    let _guard = serial();
    for parent in [false, true] {
        let root = tempfile::tempdir().unwrap();
        crash(root.path(), false);
        let name = staged_name(root.path());
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join(name.file_name().unwrap()), b"outside").unwrap();
        if parent {
            fs::rename(root.path().join("nested"), root.path().join("moved")).unwrap();
            std::os::unix::fs::symlink(outside.path(), root.path().join("nested")).unwrap();
        } else {
            fs::remove_file(&name).unwrap();
            std::os::unix::fs::symlink(outside.path().join(name.file_name().unwrap()), &name)
                .unwrap();
        }
        let _export = Export::exclusive(root.path()).unwrap();
        assert_eq!(
            fs::read(outside.path().join(name.file_name().unwrap())).unwrap(),
            b"outside"
        );
        assert_eq!(count(root.path()), 1);
    }
}

#[test]
fn recovery_batches_records_and_does_not_follow_injected_paths() {
    let _guard = serial();
    let root = tempfile::tempdir().unwrap();
    drop(Export::exclusive(root.path()).unwrap());
    let metadata = fs::metadata(root.path()).unwrap();
    let mut db = rusqlite::Connection::open(root.path().join(STATE)).unwrap();
    let transaction = db.transaction().unwrap();
    for index in 0..260 {
        transaction
            .execute(
                "INSERT INTO uploads(parent,name,parent_device,parent_inode) VALUES (?1,?2,?3,?4)",
                rusqlite::params![
                    if index == 0 { "../outside" } else { "" },
                    format!(".itookit-upload-{index:032x}"),
                    metadata.dev() as i64,
                    metadata.ino() as i64
                ],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(db);
    let _export = Export::exclusive(root.path()).unwrap();
    assert_eq!(count(root.path()), 1);
}

#[test]
fn readonly_open_does_not_create_a_journal_and_active_uploads_retain_the_writer_lock() {
    let _guard = serial();
    let root = tempfile::tempdir().unwrap();
    drop(Export::open(root.path()).unwrap());
    assert!(!root.path().join(".itookit-upload-state").exists());
    let export = Export::exclusive(root.path()).unwrap();
    let upload = export.stage("file").unwrap();
    drop(export);
    assert!(Export::exclusive(root.path()).is_err());
    drop(upload);
    assert!(Export::exclusive(root.path()).is_ok());
}

#[test]
fn a_symlinked_or_corrupt_journal_fails_without_touching_outside_data() {
    let _guard = serial();
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let state = root.path().join(".itookit-upload-state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(outside.path().join("data"), b"outside").unwrap();
    std::os::unix::fs::symlink(outside.path().join("data"), root.path().join(STATE)).unwrap();
    assert!(Export::exclusive(root.path()).is_err());
    assert_eq!(fs::read(outside.path().join("data")).unwrap(), b"outside");
    fs::remove_file(root.path().join(STATE)).unwrap();
    drop(Export::exclusive(root.path()).unwrap());
    fs::write(root.path().join(STATE), b"corrupted").unwrap();
    assert_eq!(
        Export::exclusive(root.path()).err().unwrap().code,
        "UPLOAD_JOURNAL_IO"
    );
}
