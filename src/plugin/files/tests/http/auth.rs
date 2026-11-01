//! Authentication and authorization at the HTTP boundary.

use crate::common::{app, body_json, request, router_for, state};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use cells::fs::Export;
use tower::ServiceExt;

#[tokio::test]
async fn password_clients_authenticate_with_basic_without_bearer_fallback() {
    let root = tempfile::tempdir().unwrap();
    let server = router_for(state(
        Export::open(root.path()).unwrap(),
        Some("alice".into()),
        vec!["docs".into()],
        vec![],
    ));
    for (value, expected) in [
        (
            format!(
                "Basic {}",
                STANDARD.encode("alice:test-secret-at-least-24-bytes")
            ),
            StatusCode::OK,
        ),
        (
            format!(
                "Basic {}",
                STANDARD.encode("bob:test-secret-at-least-24-bytes")
            ),
            StatusCode::UNAUTHORIZED,
        ),
        (
            format!("Basic {}", STANDARD.encode("alice:wrong")),
            StatusCode::UNAUTHORIZED,
        ),
        ("Basic invalid!".into(), StatusCode::UNAUTHORIZED),
        (
            "Bearer test-secret-at-least-24-bytes".into(),
            StatusCode::UNAUTHORIZED,
        ),
        // RFC 7235: the scheme name is case-insensitive.
        (
            format!(
                "basic {}",
                STANDARD.encode("alice:test-secret-at-least-24-bytes")
            ),
            StatusCode::OK,
        ),
    ] {
        let response = server
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/exports")
                    .header("authorization", value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}

/// Authentication is required on every route, including discovery.
#[tokio::test]
async fn unauthenticated_requests_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), "content").unwrap();
    let server = app(root.path());
    for uri in [
        "/v1/capabilities",
        "/v1/exports",
        "/v1/fs/docs/entries?path=",
        "/v1/fs/docs/content?path=file",
    ] {
        let response = server
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
    // An unknown alias is forbidden, not silently empty.
    let response = server
        .oneshot(
            request("/v1/fs/unknown/content?path=file")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "EACCES");
}
