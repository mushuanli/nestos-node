//! `PUT /v1/fs/:alias/content` and `POST /v1/fs/:alias/mutate`.

use crate::common::{app_export, body_json, mutate_request, request};
use axum::{body::Body, http::StatusCode};
use cells::fs::Export;
use std::os::unix::fs::PermissionsExt;
use tower::ServiceExt;

fn writable(root: &std::path::Path) -> axum::Router {
    app_export(Export::exclusive(root).unwrap())
}

fn has_staged_file(root: &std::path::Path) -> bool {
    std::fs::read_dir(root).unwrap().any(|entry| {
        let entry = entry.unwrap();
        entry.file_type().unwrap().is_file()
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with(".itookit-upload-")
    })
}

#[tokio::test]
async fn conditional_replace_serializes_writers_and_never_replays_ids() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note"), "original").unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    assert!(Export::exclusive(root.path()).is_err());
    let server = app_export(export);
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/content?path=note")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let revision = response.headers()["etag"].clone();
    let put = |id: &str, text: &str| {
        request("/v1/fs/docs/content?path=note")
            .method("PUT")
            .header("if-match", &revision)
            .header("x-operation-id", id)
            .body(Body::from(text.to_owned()))
            .unwrap()
    };
    let (a, b) = tokio::join!(
        server.clone().oneshot(put("first", "one")),
        server.clone().oneshot(put("second", "two"))
    );
    let mut codes = [a.unwrap().status().as_u16(), b.unwrap().status().as_u16()];
    codes.sort();
    assert_eq!(codes, [200, 412]);
    let current = std::fs::read(root.path().join("note")).unwrap();
    let duplicate = server
        .clone()
        .oneshot(put("first", "different body"))
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    assert_eq!(std::fs::read(root.path().join("note")).unwrap(), current);
    let status = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/operations/first")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let cancelled = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/operations/before/cancel")
                .method("POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cancelled.status(), StatusCode::OK);
    assert_eq!(
        server
            .oneshot(put("before", "cancelled"))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert!(!has_staged_file(root.path()));
}

#[tokio::test]
async fn a_write_without_a_precondition_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let server = writable(root.path());
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/content?path=x")
                .method("PUT")
                .header("x-operation-id", "no-condition")
                .body(Body::from("x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
    // Both conditions at once is equally ambiguous.
    let response = server
        .oneshot(
            request("/v1/fs/docs/content?path=x")
                .method("PUT")
                .header("if-match", "\"r\"")
                .header("if-none-match", "*")
                .header("x-operation-id", "both")
                .body(Body::from("x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
}

#[tokio::test]
async fn creation_and_structure_operations_respect_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let server = writable(root.path());
    for (id, expected) in [
        ("create", StatusCode::OK),
        ("collision", StatusCode::PRECONDITION_FAILED),
    ] {
        let response = server
            .clone()
            .oneshot(
                request("/v1/fs/docs/content?path=new")
                    .method("PUT")
                    .header("if-none-match", "*")
                    .header("x-operation-id", id)
                    .body(Body::from("new bytes"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    for (id, body, expected) in [
        (
            "dir",
            r#"{"action":"mkdir","path":"folder"}"#,
            StatusCode::OK,
        ),
        (
            "move",
            r#"{"action":"rename","path":"new","to":"folder/moved"}"#,
            StatusCode::OK,
        ),
        (
            "escape",
            r#"{"action":"rename","path":"folder/moved","to":"../escape"}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "nonempty",
            r#"{"action":"remove","path":"folder"}"#,
            StatusCode::CONFLICT,
        ),
        (
            "remove",
            r#"{"action":"remove","path":"folder/moved"}"#,
            StatusCode::OK,
        ),
        (
            "recursive",
            r#"{"action":"remove","path":"folder","recursive":true}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "unknown-action",
            r#"{"action":"chmod","path":"folder"}"#,
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = server
            .clone()
            .oneshot(mutate_request(id, body))
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{id}");
    }
}

/// A conditional PUT must not silently narrow the permissions of the file it
/// replaces, and a created file must not be born private.
#[tokio::test]
async fn replacements_preserve_file_modes() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("note");
    std::fs::write(&target, "original").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
    let server = writable(root.path());
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/content?path=note")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let revision = response.headers()["etag"].clone();
    let replaced = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/content?path=note")
                .method("PUT")
                .header("if-match", &revision)
                .header("x-operation-id", "replace")
                .body(Body::from("second"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(replaced.status(), StatusCode::OK);
    assert_eq!(
        target.metadata().unwrap().permissions().mode() & 0o777,
        0o640
    );

    let created = server
        .oneshot(
            request("/v1/fs/docs/content?path=fresh")
                .method("PUT")
                .header("if-none-match", "*")
                .header("x-operation-id", "fresh")
                .body(Body::from("bytes"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    assert_eq!(
        root.path()
            .join("fresh")
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
}

#[tokio::test]
async fn cancellation_during_upload_cleans_staging_and_never_publishes() {
    let root = tempfile::tempdir().unwrap();
    let server = writable(root.path());
    let slow = futures_util::stream::once(async { Ok::<_, std::io::Error>("first bytes") })
        .chain(futures_util::stream::pending());
    use futures_util::StreamExt;
    let upload = tokio::spawn(
        server.clone().oneshot(
            request("/v1/fs/docs/content?path=cancelled")
                .method("PUT")
                .header("if-none-match", "*")
                .header("x-operation-id", "uploading")
                .body(Body::from_stream(slow))
                .unwrap(),
        ),
    );
    for _ in 0..100 {
        if has_staged_file(root.path()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(has_staged_file(root.path()));
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/operations/uploading/cancel")
                .method("POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        upload.await.unwrap().unwrap().status(),
        StatusCode::REQUEST_TIMEOUT
    );
    assert!(!has_staged_file(root.path()));
    assert!(!root.path().join("cancelled").exists());
    let response = server
        .oneshot(
            request("/v1/fs/docs/operations/uploading")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(response).await["outcome"], "not-committed");
}

/// A write to a read-only export is refused before the body is consumed.
#[tokio::test]
async fn read_only_exports_refuse_mutations() {
    let root = tempfile::tempdir().unwrap();
    let server = crate::common::app(root.path());
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/content?path=x")
                .method("PUT")
                .header("if-none-match", "*")
                .header("x-operation-id", "ro-put")
                .body(Body::from("x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "EROFS");
    let response = server
        .oneshot(mutate_request(
            "ro-mutate",
            r#"{"action":"mkdir","path":"d"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
