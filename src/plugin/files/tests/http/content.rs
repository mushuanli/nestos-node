//! `GET /v1/fs/:alias/content` — bytes, ranges and conditional reads.

use crate::common::{app, app_export, body_bytes, body_json, request};
use axum::{body::Body, http::StatusCode};
use cells::fs::Export;
use tower::ServiceExt;

#[tokio::test]
async fn reads_binary_content_and_sets_no_store() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.bin"), [0, 255, 1, 128]).unwrap();
    let response = app(root.path())
        .oneshot(
            request("/v1/fs/docs/content?path=file.bin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["accept-ranges"], "bytes");
    assert_eq!(body_bytes(response).await, [0, 255, 1, 128]);
}

#[tokio::test]
async fn traversal_and_links_cannot_read_outside_export() {
    let root = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/etc", root.path().join("escape")).unwrap();
    for path in ["..%2Fetc%2Fpasswd", "%2Fetc%2Fpasswd", "escape%2Fpasswd"] {
        let response = app(root.path())
            .oneshot(
                request(&format!("/v1/fs/docs/content?path={path}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(!response.status().is_success(), "{path}");
    }
    let response = app(root.path())
        .oneshot(
            request("/v1/fs/docs/stat")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"paths":["escape","missing","escape/passwd"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let data = body_json(response).await;
    assert_eq!(data["results"][0]["stat"]["kind"], "symlink");
    assert!(data["results"][1]["stat"].is_null());
    assert_eq!(data["results"][2]["error"], "EACCES");
    // A platform absolute path is not an export-relative path.
    assert_eq!(
        app(root.path())
            .oneshot(
                request("/v1/fs/docs/content?path=C%3A%2Fhost")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn ranges_are_resolved_ignored_or_rejected_explicitly() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("bytes"), b"0123456789").unwrap();
    // Range behaviour is exercised on a writable export so validators exist.
    let server = app_export(Export::exclusive(root.path()).unwrap());
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/content?path=bytes")
                .header("range", "bytes=3-5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["content-range"], "bytes 3-5/10");
    assert_eq!(body_bytes(response).await, b"345");
    // An unsupported unit is ignored, not rejected.
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/content?path=bytes")
                .header("range", "items=0-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // An unsatisfiable range reports the resource size.
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/content?path=bytes")
                .header("range", "bytes=99-")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()["content-range"], "bytes */10");
    // A stale validator on a writable export is a conflict.
    let response = server
        .oneshot(
            request("/v1/fs/docs/content?path=bytes")
                .header("if-match", "\"old\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
}

/// A read-only export has no validator, so `If-Match` cannot be evaluated and
/// must not turn every conditional read into a conflict.
#[tokio::test]
async fn conditional_read_on_a_read_only_export_is_served() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("bytes"), b"0123456789").unwrap();
    let response = app(root.path())
        .oneshot(
            request("/v1/fs/docs/content?path=bytes")
                .header("if-match", "\"old\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, b"0123456789");
}

#[tokio::test]
async fn directories_and_missing_files_have_distinct_failures() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("folder")).unwrap();
    let server = app(root.path());
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/content?path=folder")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(response).await["code"], "ECAPABILITY");
    let response = server
        .oneshot(
            request("/v1/fs/docs/content?path=missing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
