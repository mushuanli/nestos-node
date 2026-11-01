//! Workspace lease fencing: policy (`lease`) over an atomic journal.

use cells::workspace::lease::{LeaseError, LeaseRegistry, Owner};

fn owner(instance: &str) -> Owner {
    Owner {
        principal: "user".into(),
        instance_id: instance.into(),
    }
}

#[test]
fn stale_owner_and_timer_cannot_release_transferred_control() {
    let directory = tempfile::tempdir().unwrap();
    let registry = LeaseRegistry::open(directory.path()).unwrap();
    let old = registry
        .create(owner("a"), "grant-1".into(), 0, 1000)
        .unwrap();
    let next = registry
        .transfer(&old, &owner("a"), owner("b"), "grant-1", 100)
        .unwrap();
    assert_eq!(
        registry.revoke(&old, &owner("a"), "grant-1", 200),
        Err(LeaseError::Stale)
    );
    assert_eq!(
        registry.renew(&old, &owner("a"), "grant-1", 200, 1000),
        Err(LeaseError::Stale)
    );
    assert_eq!(
        registry
            .reaper()
            .expire(&old.workspace_id, old.lease_generation, 2000),
        Ok(false)
    );
    assert_eq!(
        registry.validate(&next, &owner("b"), "grant-1", 999),
        Ok(())
    );
    assert_eq!(
        registry.validate(&next, &owner("b"), "grant-2", 999),
        Err(LeaseError::Stale)
    );
    assert_eq!(
        registry
            .reaper()
            .expire(&next.workspace_id, next.lease_generation, 1000),
        Ok(true)
    );
    assert_eq!(
        registry.renew(&next, &owner("b"), "grant-1", 1000, 1000),
        Err(LeaseError::Stale)
    );
}

#[test]
fn restart_revokes_tokens_and_preserves_unknown_workspaces_for_reconciliation() {
    let directory = tempfile::tempdir().unwrap();
    let handle = {
        let registry = LeaseRegistry::open(directory.path()).unwrap();
        assert!(LeaseRegistry::open(directory.path()).is_err());
        registry
            .create(owner("a"), "grant".into(), 0, 1000)
            .unwrap()
    };
    let raw = std::fs::read_to_string(directory.path().join("leases.json")).unwrap();
    assert!(!raw.contains(&handle.lease_id));
    let registry = LeaseRegistry::open(directory.path()).unwrap();
    assert_eq!(
        registry.validate(&handle, &owner("a"), "grant", 1),
        Err(LeaseError::Stale)
    );
    assert_eq!(
        registry.reaper().pending_recovery().unwrap(),
        vec![(handle.workspace_id.clone(), handle.lease_generation)]
    );
    registry
        .reaper()
        .complete_release(&handle.workspace_id, handle.lease_generation)
        .unwrap();
    assert!(registry.reaper().pending_recovery().unwrap().is_empty());
}

#[test]
fn corrupt_journals_are_not_reset_and_tokens_do_not_authorize_another_principal() {
    let directory = tempfile::tempdir().unwrap();
    {
        let registry = LeaseRegistry::open(directory.path()).unwrap();
        let handle = registry
            .create(owner("a"), "grant".into(), 0, 1000)
            .unwrap();
        let other = Owner {
            principal: "other".into(),
            instance_id: "a".into(),
        };
        assert_eq!(
            registry.validate(&handle, &other, "grant", 1),
            Err(LeaseError::Stale)
        );
    }
    std::fs::write(directory.path().join("leases.json"), "broken").unwrap();
    assert!(LeaseRegistry::open(directory.path()).is_err());
    assert_eq!(
        std::fs::read_to_string(directory.path().join("leases.json")).unwrap(),
        "broken"
    );
}

#[test]
fn release_retries_are_idempotent_but_do_not_accept_a_rotated_token() {
    let directory = tempfile::tempdir().unwrap();
    let registry = LeaseRegistry::open(directory.path()).unwrap();
    let handle = registry
        .create(owner("a"), "grant".into(), 0, 1000)
        .unwrap();
    registry.revoke(&handle, &owner("a"), "grant", 100).unwrap();
    registry
        .revoke(&handle, &owner("a"), "grant", 2000)
        .unwrap();
    registry
        .reaper()
        .complete_release(&handle.workspace_id, handle.lease_generation)
        .unwrap();
    registry
        .reaper()
        .complete_release(&handle.workspace_id, handle.lease_generation)
        .unwrap();
    registry
        .revoke(&handle, &owner("a"), "grant", 2000)
        .unwrap();
    assert_eq!(
        registry.with_current(&handle, &owner("a"), "grant", 100, || panic!(
            "stale effect ran"
        )),
        Err::<(), _>(LeaseError::Stale)
    );
}

/// A persist that fails before the rename leaves disk and memory in agreement,
/// so the registry stays usable instead of being permanently poisoned.
#[test]
fn persistence_failure_before_the_rename_is_retryable() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("registry");
    let registry = LeaseRegistry::open(&path).unwrap();
    let handle = registry
        .create(owner("a"), "grant".into(), 0, 1000)
        .unwrap();
    // With the directory gone, the temporary document cannot be created.
    std::fs::rename(&path, parent.path().join("moved")).unwrap();
    assert_eq!(
        registry.renew(&handle, &owner("a"), "grant", 100, 1000),
        Err(LeaseError::Storage)
    );
    std::fs::rename(parent.path().join("moved"), &path).unwrap();
    // The old image is still authoritative, and the next attempt succeeds.
    assert_eq!(
        registry.validate(&handle, &owner("a"), "grant", 100),
        Ok(())
    );
    assert_eq!(
        registry.renew(&handle, &owner("a"), "grant", 100, 1000),
        Ok(())
    );
}

#[test]
fn released_records_are_reclaimed_by_the_next_creation() {
    let directory = tempfile::tempdir().unwrap();
    let registry = LeaseRegistry::open(directory.path()).unwrap();
    let first = registry
        .create(owner("a"), "grant".into(), 0, 1000)
        .unwrap();
    registry.revoke(&first, &owner("a"), "grant", 100).unwrap();
    registry
        .reaper()
        .complete_release(&first.workspace_id, first.lease_generation)
        .unwrap();
    registry
        .create(owner("b"), "grant".into(), 0, 1000)
        .unwrap();
    // The released record was reclaimed rather than accumulated.
    assert_eq!(registry.reaper().retained().unwrap(), 1);
    registry
        .create(owner("c"), "grant".into(), 0, 1000)
        .unwrap();
    assert_eq!(registry.reaper().retained().unwrap(), 2);
}

#[test]
fn crash_orphaned_temporary_documents_are_swept_on_open() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("leases.deadbeef.tmp"), "{}").unwrap();
    let registry = LeaseRegistry::open(directory.path()).unwrap();
    assert!(!directory.path().join("leases.deadbeef.tmp").exists());
    assert_eq!(registry.reaper().retained().unwrap(), 0);
}
