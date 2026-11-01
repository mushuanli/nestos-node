use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use cells::{app::State, projects};
use serde_json::{json, Value};
use tower::ServiceExt;
#[allow(dead_code)]
#[path = "support/directory_acl.rs"]
mod support;
use support::*;
#[tokio::test]
async fn public_and_named_readers_get_only_their_directory_capabilities() {
    let (_root, state) = fixture();
    let public = exports(&state, None).await;
    assert_eq!(public["exports"].as_array().unwrap().len(), 1);
    assert_eq!(public["exports"][0]["access"], "ro");
    assert_eq!(
        public["exports"][0]["authorization"]["principalId"],
        "anonymous"
    );
    assert_eq!(
        exports(&state, Some(ALICE)).await["exports"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        exports(&state, Some(BOB)).await["exports"][0]["access"],
        "ro"
    );
    assert_eq!(
        request(
            &state,
            None,
            "GET",
            "/v1/fs/docs/content?path=readme.txt",
            json!(null)
        )
        .await
        .1,
        b"public fixture"
    );
    assert_eq!(
        request(
            &state,
            None,
            "GET",
            "/v1/fs/private/content?path=secret.txt",
            json!(null)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &state,
            Some("invalid-token"),
            "GET",
            "/v1/exports",
            json!(null)
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn read_only_is_enforced_for_direct_mutations_and_seqfile_without_project_headers() {
    let (root, state) = fixture();
    let initial_entries = std::fs::read_dir(root.path().join("docs")).unwrap().count();
    for token in [None, Some(BOB)] {
        for change in [
            json!({"action":"mkdir","path":"created"}),
            json!({"action":"rename","path":"readme.txt","to":"renamed.txt"}),
            json!({"action":"remove","path":"readme.txt"}),
        ] {
            assert_eq!(
                request(&state, token, "POST", "/v1/fs/docs/mutate", change)
                    .await
                    .0,
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            request(
                &state,
                token,
                "PUT",
                "/v1/fs/docs/content?path=new.txt",
                json!("new")
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                &state,
                token,
                "POST",
                "/v1/fs/docs/seq/transaction",
                json!({"path":"test.seq","expectedRevision":null,"changes":[]})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        std::fs::read(root.path().join("docs/readme.txt")).unwrap(),
        b"public fixture"
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("docs")).unwrap().count(),
        initial_entries
    );
}
#[tokio::test]
async fn write_grants_do_not_imply_execution_and_anonymous_cannot_allocate_projects() {
    let (root, state) = fixture();
    let req = Request::builder()
        .method("PUT")
        .uri("/v1/fs/docs/content?path=new.txt")
        .header("authorization", format!("Bearer {ALICE}"))
        .header("if-none-match", "*")
        .header("x-operation-id", "acl-write-1")
        .body(Body::from("created"))
        .unwrap();
    let response = cells::router(state.clone(), &[])
        .unwrap()
        .oneshot(req)
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    assert_eq!(
        std::fs::read(root.path().join("docs/new.txt")).unwrap(),
        b"created"
    );
    let who = state.auth.index("alice").unwrap();
    assert!(!state.auth.client(who).may_execute("docs"));
    let anonymous = state.auth.index("anonymous").unwrap();
    assert_eq!(
        projects::call(
            &state,
            anonymous,
            "project_register",
            json!({"name":"public","alias":"docs","path":"","access":"ro"})
        )
        .await
        .unwrap_err()
        .code,
        "EACCES"
    );
}
#[tokio::test]
async fn stale_authorization_and_forged_identity_are_rejected_before_access() {
    let (_root, state) = fixture();
    for (header, value) in [
        ("x-cells-principal", "bob"),
        ("x-cells-grant-revision", "old"),
    ] {
        let req = Request::builder()
            .uri("/v1/fs/docs/content?path=readme.txt")
            .header("authorization", format!("Bearer {ALICE}"))
            .header(header, value)
            .body(Body::empty())
            .unwrap();
        let response = cells::router(state.clone(), &[])
            .unwrap()
            .oneshot(req)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&bytes).contains("AUTHORIZATION_CHANGED"));
    }
}
#[tokio::test]
async fn project_ownership_survives_reordering_and_credential_rotation() {
    let (root, state) = fixture();
    let alice = state.auth.index("alice").unwrap();
    let project = projects::call(
        &state,
        alice,
        "project_register",
        json!({"name":"Alice","alias":"docs","path":"","access":"ro"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    assert_eq!(project["owner"], "alice");
    drop(state);
    let mut next = config(root.path(), true);
    next.principals[1].token = Some("rotated-alice-token-0123456789".into());
    let state = State::from_config(&next).unwrap();
    let alice = state.auth.index("alice").unwrap();
    assert_eq!(alice, 1);
    assert!(projects::call(
        &state,
        alice,
        "project_read",
        json!({"projectId":project["id"]})
    )
    .await
    .is_ok());
    let bob = state.auth.index("bob").unwrap();
    assert!(projects::call(
        &state,
        bob,
        "project_read",
        json!({"projectId":project["id"]})
    )
    .await
    .is_err());
}
#[test]
fn rejects_unknown_subjects_duplicate_credentials_and_privilege_escalation() {
    let (root, state) = fixture();
    drop(state);
    for mutation in 0..4 {
        let mut c = config(root.path(), false);
        match mutation {
            0 => c.exports[0].grants[0].principal = "missing".into(),
            1 => c.principals[1].token = Some(ALICE.into()),
            2 => c.exports[0].access = cells::config::Access::Ro,
            _ => c.principals[0].id = "anonymous".into(),
        }
        assert!(State::from_config(&c).is_err());
    }
}

#[tokio::test]
async fn downgrading_a_writer_preserves_reads_and_projects_without_keeping_old_write_grants() {
    let (root, state) = fixture();
    let alice = state.auth.index("alice").unwrap();
    let project = projects::call(
        &state,
        alice,
        "project_register",
        json!({"name":"Alice","alias":"docs","path":"","access":"rw"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    let before = state.auth.client(alice).authorization();
    drop(state);
    let mut c = config(root.path(), false);
    c.exports[0].grants[0].access = cells::config::Access::Ro;
    let state = State::from_config(&c).unwrap();
    let alice = state.auth.index("alice").unwrap();
    let read = projects::call(
        &state,
        alice,
        "project_read",
        json!({"projectId":project["id"]}),
    )
    .await
    .unwrap();
    assert_eq!(read["project"]["access"], "ro");
    assert_eq!(read["project"]["capabilities"]["fileWrite"], false);
    assert_ne!(
        before["revision"],
        read["project"]["authorization"]["revision"]
    );
    assert_eq!(
        request(
            &state,
            Some(ALICE),
            "PUT",
            "/v1/fs/docs/content?path=new.txt",
            json!("new")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn harness_profiles_events_and_receipts_are_not_shared_with_directory_readers() {
    let (root, state) = fixture();
    drop(state);
    for home in ["alice-home", "bob-home"] {
        std::fs::create_dir(root.path().join(home)).unwrap();
    }
    let mut c = config(root.path(), false);
    let profile = |id: &str, owner: &str, home: &str| {
        toml::from_str(&format!("id='{id}'\nkind='codex'\ncommand='codex'\nprincipal_id='{owner}'\nprojects=true\nhome={:?}",root.path().join(home))).unwrap()
    };
    c.harnesses = vec![
        profile("alice-model", "alice", "alice-home"),
        profile("bob-model", "bob", "bob-home"),
    ];
    let state = State::from_config(&c).unwrap();
    for id in ["alice", "bob", "anonymous"] {
        let who = state.auth.index(id).unwrap();
        let result = state
            .services
            .require::<cells::harness::Harnesses>()
            .unwrap()
            .authorized_call(&state, who, "harness_profiles", json!({}))
            .await
            .unwrap();
        assert_eq!(
            result["profiles"].as_array().unwrap().len(),
            if id == "anonymous" { 0 } else { 1 }
        );
        if id != "anonymous" {
            assert_eq!(result["profiles"][0]["id"], format!("{id}-model"));
        }
        for method in ["harness_operation", "harness_session_read", "harness_turn"] {
            assert!(state
                .services
                .require::<cells::harness::Harnesses>()
                .unwrap()
                .authorized_call(
                    &state,
                    who,
                    method,
                    json!({"profileId":"alice-model","requestId":"foreign"})
                )
                .await
                .is_err());
        }
    }
    state
        .services
        .require::<cells::harness::Harnesses>()
        .unwrap()
        .close()
        .await;
    drop(state);
    c.harnesses[1].home = c.harnesses[0].home.clone();
    assert!(State::from_config(&c).is_err());
}
#[tokio::test]
async fn legacy_numeric_project_owners_migrate_to_owner_without_accepting_other_numeric_identities()
{
    let (root, state) = fixture();
    drop(state);
    let mut c = config(root.path(), false);
    c.principals[0].id = "owner".into();
    c.exports[0].grants[0].principal = "owner".into();
    c.exports[1].grants[0].principal = "owner".into();
    let state = State::from_config(&c).unwrap();
    let p = projects::call(
        &state,
        state.auth.index("owner").unwrap(),
        "project_register",
        json!({"name":"old","alias":"docs","path":"","access":"ro"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    drop(state);
    let catalog = root.path().join("catalog/projects.json");
    let mut image: Value = serde_json::from_slice(&std::fs::read(&catalog).unwrap()).unwrap();
    image["projects"][p["id"].as_str().unwrap()]["owner"] = json!(0);
    let _: cells::projects::model::Project =
        serde_json::from_value(image["projects"][p["id"].as_str().unwrap()].clone()).unwrap();
    std::fs::write(&catalog, serde_json::to_vec(&image).unwrap()).unwrap();
    let state = State::from_config(&c).unwrap();
    let p = projects::call(
        &state,
        state.auth.index("owner").unwrap(),
        "project_read",
        json!({"projectId":p["id"]}),
    )
    .await
    .unwrap();
    assert_eq!(p["project"]["owner"], "owner");
    drop(state);
    image["projects"][p["project"]["id"].as_str().unwrap()]["owner"] = json!(1);
    std::fs::write(&catalog, serde_json::to_vec(&image).unwrap()).unwrap();
    assert!(State::from_config(&c).is_err());
}

#[tokio::test]
async fn revoked_write_permission_cannot_be_reused_by_a_persisted_directory_sync_project() {
    let (root, state) = fixture();
    drop(state);
    let mut c = config(root.path(), false);
    c.sync = Some(cells::sync::Config {
        enabled: true,
        root: root.path().join("sync"),
        principal_id: "alice".into(),
        ..Default::default()
    });
    let state = State::from_config(&c).unwrap();
    let who = state.auth.index("alice").unwrap();
    let p = projects::call(
        &state,
        who,
        "project_register",
        json!({"name":"Sync","alias":"docs","path":"","access":"rw"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    drop(state);
    c.exports[0].grants[0].access = cells::config::Access::Ro;
    let state = State::from_config(&c).unwrap();
    let result = projects::call(
        &state,
        state.auth.index("alice").unwrap(),
        "project_sync_directories",
        json!({"projectId":p["id"],"revision":p["revision"],"path":""}),
    )
    .await;
    assert_eq!(result.unwrap_err().code, "EROFS");
}
