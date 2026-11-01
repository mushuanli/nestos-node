#[cfg(feature = "sync-fault-injection")]
mod cases {
    use super::super::support::*;
    use cells::sync::SyncService;
    use serde_json::json;

    #[test]
    fn child() {
        let Ok(root) = std::env::var("CELLS_SYNC_CHILD_ROOT") else {
            return;
        };
        if std::env::var("CELLS_SYNC_CRASH_AT").ok().as_deref() == Some("after-restore-copy")
        {
            cells::sync::admin(&[
                "restore".into(),
                std::env::var("CELLS_SYNC_CHILD_CONFIG").unwrap(),
                std::env::var("CELLS_SYNC_CHILD_BACKUP").unwrap(),
            ])
            .unwrap();
            return;
        }
        let s = SyncService::open(&config(std::path::Path::new(&root))).unwrap();
        if std::env::var("CELLS_SYNC_IO_FAIL_AT").is_ok() {
            let next = std::env::var("CELLS_SYNC_CHILD_MANIFEST").unwrap();
            let head = s.head("p", "files").unwrap();
            let point = std::env::var("CELLS_SYNC_IO_FAIL_AT").unwrap();
            let generation = if point == "before-savepoint-rollback" {
                json!("999")
            } else {
                head["generation"].clone()
            };
            let result=s.command("projects/p/datasets/files/publish",&command(&s,"A",3,json!({"expectedHead":{"generation":generation,"manifestHash":head["manifestHash"]},"nextManifestHash":next})),false);
            assert!(result.unwrap_err().unknown);
            assert!(!s.healthy());
            let receipt = s.operation("A", "3").unwrap();
            if point == "publish-commit-unknown" {
                assert_eq!(receipt["outcome"], "committed");
            } else {
                assert_eq!(receipt["state"], "pending");
            }
            return;
        }
        let point = std::env::var("CELLS_SYNC_CRASH_AT").unwrap();
        if point == "after-gc-mark" {
            s.gc().unwrap();
        } else if point == "after-repair-intent" {
            s.repair(
                "p",
                &hash(b"first"),
                std::path::Path::new(&std::env::var("CELLS_SYNC_CHILD_SOURCE").unwrap()),
            )
            .unwrap();
        } else if point.contains("object") {
            install(&s, "p", b"new-object");
        } else {
            let next = std::env::var("CELLS_SYNC_CHILD_MANIFEST").unwrap();
            let current = s.head("p", "files").unwrap();
            let head =
                json!({"generation":current["generation"],"manifestHash":current["manifestHash"]});
            s.command(
                "projects/p/datasets/files/publish",
                &command(
                    &s,
                    "A",
                    3,
                    json!({"expectedHead":head,"nextManifestHash":next}),
                ),
                false,
            )
            .unwrap();
        }
    }
    #[test]
    fn rollback_and_receipt_io_failures_keep_recoverable_admission() {
        for point in ["before-savepoint-rollback", "before-receipt-write"] {
            let root = tempfile::tempdir().unwrap();
            let s = service(root.path());
            project(&s);
            let first = manifest(&s, "first");
            dataset(&s, &first);
            let next = manifest(&s, "next");
            drop(s);
            let exit = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "crash::cases::child"])
                .env("CELLS_SYNC_CHILD_ROOT", root.path())
                .env("CELLS_SYNC_IO_FAIL_AT", point)
                .env("CELLS_SYNC_CHILD_MANIFEST", next)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(exit.success());
            let s = SyncService::open(&config(root.path())).unwrap();
            assert_eq!(s.head("p", "files").unwrap()["generation"], "1");
            assert_eq!(s.operation("A", "3").unwrap()["code"], "SERVER_RESTART");
        }
    }
    fn crash(root: &std::path::Path, point: &str, manifest: &str) {
        let exit = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash::cases::child"])
            .env("CELLS_SYNC_CHILD_ROOT", root)
            .env("CELLS_SYNC_CRASH_AT", point)
            .env("CELLS_SYNC_CHILD_MANIFEST", manifest)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(exit.code(), Some(86), "{point}");
    }
    #[test]
    fn publish_crashes_leave_one_atomic_outcome() {
        for point in [
            "after-admission",
            "before-publish-commit",
            "after-publish-commit",
        ] {
            let root = tempfile::tempdir().unwrap();
            let s = service(root.path());
            project(&s);
            let first = manifest(&s, "first");
            dataset(&s, &first);
            let next = manifest(&s, "next");
            let epoch = s.epoch().to_owned();
            drop(s);
            crash(root.path(), point, &next);
            let s = SyncService::open(&config(root.path())).unwrap();
            assert_eq!(s.epoch(), epoch);
            let committed = point == "after-publish-commit";
            assert_eq!(
                s.head("p", "files").unwrap()["generation"],
                if committed { "2" } else { "1" }
            );
            let result = s.operation("A", "3").unwrap();
            assert_eq!(
                result["outcome"],
                if committed {
                    "committed"
                } else {
                    "not-committed"
                }
            );
            let retry = command(
                &s,
                "A",
                3,
                json!({"expectedHead":{"generation":"1","manifestHash":first},"nextManifestHash":next}),
            );
            assert_eq!(
                s.command("projects/p/datasets/files/publish", &retry, false)
                    .unwrap(),
                result
            );
            s.verify().unwrap();
        }
    }
    #[test]
    fn install_crashes_never_publish_or_lose_existing_head() {
        for point in ["after-object-install", "after-object-ready"] {
            let root = tempfile::tempdir().unwrap();
            let s = service(root.path());
            project(&s);
            let first = manifest(&s, "first");
            dataset(&s, &first);
            drop(s);
            crash(root.path(), point, "");
            let s = SyncService::open(&config(root.path())).unwrap();
            assert_eq!(s.head("p", "files").unwrap()["generation"], "1");
            assert!(std::fs::read_dir(root.path().join("staging"))
                .unwrap()
                .next()
                .is_none());
            install(&s, "p", b"new-object");
            s.verify().unwrap();
        }
    }
    #[test]
    fn commit_uncertainty_never_becomes_a_false_failure() {
        let root = tempfile::tempdir().unwrap();
        let s = service(root.path());
        project(&s);
        let first = manifest(&s, "first");
        dataset(&s, &first);
        let next = manifest(&s, "next");
        drop(s);
        let exit = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash::cases::child"])
            .env("CELLS_SYNC_CHILD_ROOT", root.path())
            .env("CELLS_SYNC_IO_FAIL_AT", "publish-commit-unknown")
            .env("CELLS_SYNC_CHILD_MANIFEST", next)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(exit.success());
        let s = SyncService::open(&config(root.path())).unwrap();
        assert!(s.healthy());
        assert_eq!(s.head("p", "files").unwrap()["generation"], "2");
        assert_eq!(s.operation("A", "3").unwrap()["outcome"], "committed");
    }
    #[test]
    fn gc_and_repair_intents_survive_process_exit() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        let s = service(&root);
        project(&s);
        let first = manifest(&s, "first");
        dataset(&s, &first);
        let orphan = install(&s, "p", b"discard");
        let db = rusqlite::Connection::open(root.join("metadata.db")).unwrap();
        db.execute("UPDATE objects SET retain_until=0 WHERE hash=?1", [&orphan])
            .unwrap();
        drop(db);
        drop(s);
        crash(&root, "after-gc-mark", "");
        let s = SyncService::open(&config(&root)).unwrap();
        assert!(s.object_file("p", &orphan).is_err());
        let h = hash(b"first");
        let path = root.join("objects/personal/p").join(&h[..2]).join(&h);
        std::fs::write(path, b"wrong").unwrap();
        assert!(s.object_file("p", &h).is_err());
        drop(s);
        let source = parent.path().join("source");
        std::fs::write(&source, b"first").unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash::cases::child"])
            .env("CELLS_SYNC_CHILD_ROOT", &root)
            .env("CELLS_SYNC_CRASH_AT", "after-repair-intent")
            .env("CELLS_SYNC_CHILD_SOURCE", source)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(result.code(), Some(86));
        let s = SyncService::open(&config(&root)).unwrap();
        s.verify().unwrap();
        assert_eq!(s.head("p", "files").unwrap()["generation"], "1");
    }
    #[test]
    fn interrupted_restore_is_fenced_and_can_resume() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        let s = service(&root);
        project(&s);
        let h = manifest(&s, "first");
        dataset(&s, &h);
        let backup = parent.path().join("backup");
        s.backup(&backup).unwrap();
        drop(s);
        let target = parent.path().join("target");
        let file = parent.path().join("restore.toml");
        std::fs::write(&file,format!("listen='127.0.0.1:0'\nexecution=false\ntoken='sync-test-secret-at-least-24-bytes'\n[sync]\nenabled=true\nroot='{}'\nmetadata_reserve_bytes=0",target.display())).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash::cases::child"])
            .env("CELLS_SYNC_CHILD_ROOT", &target)
            .env("CELLS_SYNC_CRASH_AT", "after-restore-copy")
            .env("CELLS_SYNC_CHILD_CONFIG", &file)
            .env("CELLS_SYNC_CHILD_BACKUP", &backup)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(result.code(), Some(86));
        assert!(SyncService::open(&config(&target)).is_err());
        cells::sync::admin(&[
            "restore".into(),
            file.to_string_lossy().into_owned(),
            backup.to_string_lossy().into_owned(),
        ])
        .unwrap();
        let s = SyncService::open(&config(&target)).unwrap();
        s.verify().unwrap();
        assert_eq!(s.head("p", "files").unwrap()["manifestHash"], h);
    }
}
