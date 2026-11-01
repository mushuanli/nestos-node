//! Export-level behaviour that does not need HTTP: capability opening, restart
//! recovery and revision retirement.

use cells::fs::{Export, Exports};

#[test]
fn exclusive_open_holds_the_lock_and_a_second_open_fails() {
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    assert!(export.writable());
    assert!(Export::exclusive(root.path()).is_err());
    // A read-only capability is still possible while the writer holds the lock.
    assert!(Export::open(root.path()).is_ok());
}

#[test]
fn restart_retires_validators_without_scanning_unregistered_uploads() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note"), "content").unwrap();
    let old = {
        let export = Export::exclusive(root.path()).unwrap();
        export.stat("note").unwrap().unwrap().revision
    };
    std::fs::write(root.path().join(".itookit-upload-abandoned"), "incomplete").unwrap();
    // An outside entry with a reserved name must not block startup.
    std::fs::create_dir(root.path().join(".itookit-upload-directory")).unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    assert_ne!(export.stat("note").unwrap().unwrap().revision, old);
    assert!(root.path().join(".itookit-upload-abandoned").exists());
    assert!(root.path().join(".itookit-upload-directory").exists());
}

/// Reserved staging names are never listed, but their siblings are.
#[test]
fn reserved_names_are_hidden_from_listings() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("visible"), "x").unwrap();
    std::fs::write(root.path().join(".itookit-upload-hidden"), "x").unwrap();
    let export = Export::open(root.path()).unwrap();
    let listing = export
        .list("", &tokio_util::sync::CancellationToken::new())
        .unwrap();
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["visible"]);
}

#[test]
fn the_registry_lists_aliases_and_can_invalidate_all_revisions() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note"), "content").unwrap();
    let writable = Export::exclusive(root.path()).unwrap();
    let reference = root.path().join("reference");
    std::fs::create_dir(&reference).unwrap();
    let exports = Exports::new(
        [
            ("project".to_owned(), std::sync::Arc::new(writable)),
            (
                "reference".to_owned(),
                std::sync::Arc::new(Export::open(&reference).unwrap()),
            ),
        ]
        .into_iter()
        .collect(),
    );
    assert_eq!(
        exports.aliases().collect::<Vec<_>>(),
        ["project", "reference"]
    );
    assert!(exports.get("project").unwrap().writable());
    assert!(!exports.get("reference").unwrap().writable());
    let before = exports
        .get("project")
        .unwrap()
        .stat("note")
        .unwrap()
        .unwrap();
    exports.invalidate_revisions();
    let after = exports
        .get("project")
        .unwrap()
        .stat("note")
        .unwrap()
        .unwrap();
    assert_ne!(before.revision, after.revision);
}

#[test]
fn file_metadata_is_lazy_remote_durable_and_mount_independent() {
    use cells::fs::mutation::Change;
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("nested")).unwrap();
    std::fs::write(root.path().join("nested/note.md"), "Original").unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    let before = export.stat("nested/note.md").unwrap().unwrap().revision;
    export
        .list("", &tokio_util::sync::CancellationToken::new())
        .unwrap();
    assert!(!root.path().join(".nestos").exists());
    export
        .apply(
            &Change::Metadata {
                path: "nested/note.md".into(),
                patch: json!({"a":1,"nested":{"id":"anchor"}})
                    .as_object()
                    .unwrap()
                    .clone(),
                remove: vec![],
            },
            || Ok(()),
        )
        .unwrap();
    assert!(root.path().join(".nestos/meta.sqlite").is_file());
    let stat = export.stat("nested/note.md").unwrap().unwrap();
    assert_eq!(stat.revision, before);
    let identity = stat.source_identity.clone();
    assert!(identity.is_some());
    assert_eq!(stat.metadata["a"], 1);
    let other = Export::open(root.path()).unwrap();
    assert_eq!(
        other.stat("nested/note.md").unwrap().unwrap().metadata["nested"]["id"],
        "anchor"
    );
    assert!(other
        .apply(
            &Change::Metadata {
                path: "nested/note.md".into(),
                patch: json!({"a":2}).as_object().unwrap().clone(),
                remove: vec![]
            },
            || Ok(())
        )
        .is_err());
    assert!(export.stat(".nestos/meta.sqlite").is_err());
    assert_eq!(
        export
            .list("", &tokio_util::sync::CancellationToken::new())
            .unwrap()
            .entries
            .len(),
        1
    );
    export
        .apply(
            &Change::Rename {
                path: "nested".into(),
                to: "moved".into(),
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(
        other.stat("moved/note.md").unwrap().unwrap().metadata["a"],
        1
    );
    export
        .apply(
            &Change::Metadata {
                path: "moved/note.md".into(),
                patch: json!({"b":true}).as_object().unwrap().clone(),
                remove: vec!["a".into()],
            },
            || Ok(()),
        )
        .unwrap();
    assert!(!other
        .stat("moved/note.md")
        .unwrap()
        .unwrap()
        .metadata
        .contains_key("a"));
    drop(other);
    drop(export);
    let reopened = Export::exclusive(root.path()).unwrap();
    assert_eq!(
        reopened
            .stat("moved/note.md")
            .unwrap()
            .unwrap()
            .source_identity,
        identity
    );
    assert_eq!(
        reopened.stat("moved/note.md").unwrap().unwrap().metadata["b"],
        true
    );
    reopened
        .apply(
            &Change::Remove {
                path: "moved/note.md".into(),
            },
            || Ok(()),
        )
        .unwrap();
    std::fs::write(root.path().join("moved/note.md"), "New").unwrap();
    assert!(reopened
        .stat("moved/note.md")
        .unwrap()
        .unwrap()
        .metadata
        .is_empty());
}
