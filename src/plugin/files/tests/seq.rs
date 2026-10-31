use nestos_node::fs::{
    seq::{self, Change, Update},
    Export,
};
use serde_json::json;

#[test]
fn sqlite_seqfile_is_portable_and_conditional_transactions_do_not_lose_updates() {
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    let snapshot = seq::snapshot(&export, "info.seq").unwrap();
    assert!(snapshot.entries.is_empty());
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: snapshot.revision,
            changes: vec![Change::Set {
                key: "identity".into(),
                value: json!({"id":"project-a"}),
            }],
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        &std::fs::read(root.path().join("info.seq")).unwrap()[..16],
        b"SQLite format 3\0"
    );
    let snapshot = seq::snapshot(&export, "info.seq").unwrap();
    assert_eq!(snapshot.entries[0].value, json!({"id":"project-a"}));
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: snapshot.revision.clone(),
            changes: vec![
                Change::Set {
                    key: "title".into(),
                    value: json!("new"),
                },
                Change::Delete {
                    key: "identity".into(),
                },
            ],
        },
        || Ok(()),
    )
    .unwrap();
    let stale = seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: snapshot.revision,
            changes: vec![Change::Set {
                key: "title".into(),
                value: json!("stale"),
            }],
        },
        || Ok(()),
    )
    .unwrap_err();
    assert_eq!(stale.code, "ECONFLICT");
    std::fs::copy(root.path().join("info.seq"), root.path().join("copy.seq")).unwrap();
    drop(export);
    let reader = Export::open(root.path()).unwrap();
    assert_eq!(
        seq::snapshot(&reader, "copy.seq").unwrap().entries[0].value,
        json!("new")
    );
    assert!(seq::snapshot(&reader, "../escape.seq").is_err());
}

#[test]
fn cancelled_or_invalid_batch_keeps_the_original_database() {
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: None,
            changes: vec![Change::Set {
                key: "original".into(),
                value: json!(1),
            }],
        },
        || Ok(()),
    )
    .unwrap();
    let snapshot = seq::snapshot(&export, "info.seq").unwrap();
    let bytes = std::fs::read(root.path().join("info.seq")).unwrap();
    assert!(seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: snapshot.revision,
            changes: vec![
                Change::Delete {
                    key: "original".into()
                },
                Change::Set {
                    key: "".into(),
                    value: json!(2)
                }
            ]
        },
        || Ok(())
    )
    .is_err());
    assert_eq!(std::fs::read(root.path().join("info.seq")).unwrap(), bytes);
    let snapshot = seq::snapshot(&export, "info.seq").unwrap();
    assert!(seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: snapshot.revision,
            changes: vec![]
        },
        || Err(nestos_node::core::error::Error::cancelled())
    )
    .is_err());
    assert_eq!(std::fs::read(root.path().join("info.seq")).unwrap(), bytes);
}

#[test]
fn reads_do_not_create_files_and_invalid_sqlite_or_symlinks_are_not_replaced() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    assert!(seq::snapshot(&export, "missing.seq")
        .unwrap()
        .entries
        .is_empty());
    assert!(!root.path().join("missing.seq").exists());
    std::fs::write(root.path().join("broken.seq"), b"not sqlite").unwrap();
    assert!(seq::snapshot(&export, "broken.seq").is_err());
    assert!(seq::update(
        &export,
        Update {
            path: "broken.seq".into(),
            expected_revision: None,
            changes: vec![]
        },
        || Ok(())
    )
    .is_err());
    assert_eq!(
        std::fs::read(root.path().join("broken.seq")).unwrap(),
        b"not sqlite"
    );
    std::fs::write(outside.path().join("secret.seq"), b"secret").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.seq"),
        root.path().join("link.seq"),
    )
    .unwrap();
    assert!(seq::snapshot(&export, "link.seq").is_err());
    assert!(seq::update(
        &export,
        Update {
            path: "link.seq".into(),
            expected_revision: None,
            changes: vec![]
        },
        || Ok(())
    )
    .is_err());
    assert_eq!(
        std::fs::read(outside.path().join("secret.seq")).unwrap(),
        b"secret"
    );
    drop(export);
    let readonly = Export::open(root.path()).unwrap();
    assert_eq!(
        seq::update(
            &readonly,
            Update {
                path: "new.seq".into(),
                expected_revision: None,
                changes: vec![]
            },
            || Ok(())
        )
        .unwrap_err()
        .code,
        "EROFS"
    );
    assert!(!root.path().join("new.seq").exists());
}

#[test]
fn concurrent_seqfile_creators_cannot_overwrite_each_other() {
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let write = |name: &str| {
            let calls = std::cell::Cell::new(0);
            seq::update(
                &export,
                Update {
                    path: "info.seq".into(),
                    expected_revision: None,
                    changes: vec![Change::Set {
                        key: "winner".into(),
                        value: json!(name),
                    }],
                },
                || {
                    calls.set(calls.get() + 1);
                    if calls.get() == 2 {
                        barrier.wait();
                    }
                    Ok(())
                },
            )
        };
        let a = scope.spawn(move || write("A"));
        let b = scope.spawn(move || write("B"));
        let results = [a.join().unwrap(), b.join().unwrap()];
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results.iter().find_map(|r| r.as_ref().err()).unwrap().code,
            "ECONFLICT"
        );
    });
    assert_eq!(seq::snapshot(&export, "info.seq").unwrap().entries.len(), 1);
}

#[test]
fn bounded_queries_seek_keys_and_page_literal_prefixes_with_revision_fencing() {
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: None,
            changes: ["event/%/01", "event/%/02", "event/_/03", "private"]
                .into_iter()
                .map(|key| Change::Set {
                    key: key.into(),
                    value: json!(key),
                })
                .collect(),
        },
        || Ok(()),
    )
    .unwrap();
    let first = seq::query(
        &export,
        "info.seq",
        serde_json::from_value(json!({"prefix":"event/%/","limit":1})).unwrap(),
    )
    .unwrap();
    assert_eq!(first.snapshot.entries[0].key, "event/%/01");
    assert_eq!(first.next.as_deref(), Some("event/%/01"));
    let next = seq::query(&export, "info.seq", serde_json::from_value(json!({"prefix":"event/%/","after":first.next,"limit":1,"expectedRevision":first.snapshot.revision})).unwrap()).unwrap();
    assert_eq!(next.snapshot.entries[0].key, "event/%/02");
    assert!(next.next.is_none());
    let selected = seq::query(
        &export,
        "info.seq",
        serde_json::from_value(json!({"keys":["event/_/03","missing"]})).unwrap(),
    )
    .unwrap();
    assert_eq!(selected.snapshot.entries.len(), 1);
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: selected.snapshot.revision.clone(),
            changes: vec![],
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        seq::query(
            &export,
            "info.seq",
            serde_json::from_value(
                json!({"keys":[],"expectedRevision":selected.snapshot.revision})
            )
            .unwrap()
        )
        .err()
        .unwrap()
        .code,
        "ECONFLICT"
    );
    for selection in [
        json!({"keys":["private"],"prefix":"event"}),
        json!({"limit":0}),
        json!({"limit":1001}),
        json!({"keys":[""]}),
    ] {
        assert!(seq::query(
            &export,
            "info.seq",
            serde_json::from_value(selection).unwrap()
        )
        .is_err());
    }
}

#[test]
fn large_databases_use_native_pages_without_replacing_the_inode() {
    use std::os::unix::fs::MetadataExt;
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    seq::update(
        &export,
        Update {
            path: "large.seq".into(),
            expected_revision: None,
            changes: vec![],
        },
        || Ok(()),
    )
    .unwrap();
    let path = root.path().join("large.seq");
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute(
            "INSERT INTO entries VALUES('large',?1)",
            [json!("x".repeat(17 * 1024 * 1024)).to_string()],
        )
        .unwrap();
    }
    let inode = std::fs::metadata(&path).unwrap().ino();
    let baseline = seq::query(
        &export,
        "large.seq",
        serde_json::from_value(json!({"keys":["small"]})).unwrap(),
    )
    .unwrap();
    seq::update(
        &export,
        Update {
            path: "large.seq".into(),
            expected_revision: baseline.snapshot.revision,
            changes: vec![Change::Set {
                key: "small".into(),
                value: json!(42),
            }],
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    let selected = seq::query(
        &export,
        "large.seq",
        serde_json::from_value(json!({"keys":["small"]})).unwrap(),
    )
    .unwrap();
    assert_eq!(selected.snapshot.entries[0].value, json!(42));
    assert_eq!(
        seq::snapshot(&export, "large.seq").err().unwrap().code,
        "SEQ_RESPONSE_LIMIT"
    );
}

#[test]
fn sqlite_sidecar_symlinks_and_hardlinks_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: None,
            changes: vec![],
        },
        || Ok(()),
    )
    .unwrap();
    let revision = seq::snapshot(&export, "info.seq").unwrap().revision;
    std::fs::write(outside.path().join("secret"), b"secret").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret"),
        root.path().join("info.seq-journal"),
    )
    .unwrap();
    assert!(seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: revision.clone(),
            changes: vec![]
        },
        || Ok(())
    )
    .is_err());
    assert_eq!(
        std::fs::read(outside.path().join("secret")).unwrap(),
        b"secret"
    );
    std::fs::remove_file(root.path().join("info.seq-journal")).unwrap();
    std::fs::hard_link(root.path().join("info.seq"), root.path().join("copy.seq")).unwrap();
    assert_eq!(
        seq::update(
            &export,
            Update {
                path: "info.seq".into(),
                expected_revision: revision,
                changes: vec![]
            },
            || Ok(())
        )
        .unwrap_err()
        .code,
        "SEQ_PERMISSIONS"
    );
}

#[test]
fn immutable_bodies_verify_content_and_can_precede_database_creation() {
    use seq::bodies::{self, BodyQuery, BodyWrite};
    use sha2::{Digest, Sha256};
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    let text = "a large but immutable body".to_owned();
    let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
    let query = BodyQuery {
        path: "info.seq".into(),
        sha256: hash.clone(),
    };
    assert!(bodies::read(&export, &query).unwrap()["text"].is_null());
    for _ in 0..2 {
        bodies::write(
            &export,
            BodyWrite {
                path: query.path.clone(),
                sha256: hash.clone(),
                text: text.clone(),
            },
            || Ok(()),
        )
        .unwrap();
    }
    assert_eq!(bodies::read(&export, &query).unwrap()["text"], text);
    assert!(!root.path().join("info.seq").exists());
    assert!(bodies::write(
        &export,
        BodyWrite {
            path: query.path.clone(),
            sha256: hash.clone(),
            text: "different".into()
        },
        || Ok(())
    )
    .is_err());
    std::fs::write(
        root.path().join(format!("info.seq.assets/{hash}.json")),
        b"tampered",
    )
    .unwrap();
    assert_eq!(
        bodies::read(&export, &query).unwrap_err().code,
        "SEQ_BODY_INTEGRITY"
    );
    assert!(bodies::read(
        &export,
        &BodyQuery {
            path: "../escape.seq".into(),
            sha256: hash
        }
    )
    .is_err());
}

#[test]
fn raw_download_is_a_stable_sqlite_backup_while_later_transactions_commit() {
    use std::io::Read;
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: None,
            changes: vec![Change::Set {
                key: "version".into(),
                value: json!(1),
            }],
        },
        || Ok(()),
    )
    .unwrap();
    let (mut download, revision) = export.read("info.seq").unwrap();
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: revision,
            changes: vec![Change::Set {
                key: "version".into(),
                value: json!(2),
            }],
        },
        || Ok(()),
    )
    .unwrap();
    let mut bytes = Vec::new();
    download.read_to_end(&mut bytes).unwrap();
    std::fs::write(root.path().join("download.seq"), bytes).unwrap();
    assert_eq!(
        seq::snapshot(&export, "download.seq").unwrap().entries[0].value,
        json!(1)
    );
    assert_eq!(
        seq::snapshot(&export, "info.seq").unwrap().entries[0].value,
        json!(2)
    );
}

#[test]
fn publishing_a_body_directory_keeps_the_database_validator_and_missing_parents_read_empty() {
    use sha2::{Digest, Sha256};
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    assert!(seq::snapshot(&export, "missing/folder/info.seq")
        .unwrap()
        .entries
        .is_empty());
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: None,
            changes: vec![],
        },
        || Ok(()),
    )
    .unwrap();
    let revision = seq::snapshot(&export, "info.seq").unwrap().revision;
    let text = "body".to_owned();
    let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
    seq::bodies::write(
        &export,
        seq::bodies::BodyWrite {
            path: "info.seq".into(),
            sha256: hash,
            text,
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        seq::snapshot(&export, "info.seq").unwrap().revision,
        revision
    );
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: revision,
            changes: vec![Change::Set {
                key: "ref".into(),
                value: json!("body"),
            }],
        },
        || Ok(()),
    )
    .unwrap();
}
