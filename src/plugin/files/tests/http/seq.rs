use crate::common::{app_export, body_json, request};
use axum::{body::Body, http::StatusCode};
use cells::fs::Export;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn seq_routes_enforce_authorization_and_write_permissions() {
    let root = tempfile::tempdir().unwrap();
    let server = app_export(Export::exclusive(root.path()).unwrap());
    let update = json!({"path":"info.seq","expectedRevision":null,"changes":[{"action":"set","key":"name","value":"Project"}]}).to_string();
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/seq/transaction")
                .method("POST")
                .header("content-type", "application/json")
                .header("x-operation-id", "seq-create")
                .body(Body::from(update.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["outcome"], "committed");
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/seq/snapshot")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"info.seq"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(response).await["entries"][0]["value"], "Project");
    let readonly = app_export(Export::open(root.path()).unwrap());
    let response = readonly
        .oneshot(
            request("/v1/fs/docs/seq/transaction")
                .method("POST")
                .header("content-type", "application/json")
                .header("x-operation-id", "readonly")
                .body(Body::from(update))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = server
        .oneshot(
            request("/v1/fs/unknown/seq/snapshot")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"info.seq"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn selective_queries_and_body_routes_enforce_project_access() {
    use sha2::{Digest, Sha256};
    let root = tempfile::tempdir().unwrap();
    let server = app_export(Export::exclusive(root.path()).unwrap());
    let text = "immutable text";
    let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
    for (route, input, operation) in [
        (
            "body/put",
            json!({"path":"info.seq","sha256":hash,"text":text}),
            "body-write",
        ),
        (
            "body",
            json!({"path":"info.seq","sha256":hash}),
            "body-read",
        ),
        (
            "query",
            json!({"path":"missing.seq","selection":{"keys":["only-this"]}}),
            "key-read",
        ),
    ] {
        let response = server
            .clone()
            .oneshot(
                request(&format!("/v1/fs/docs/seq/{route}"))
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("x-operation-id", operation)
                    .body(Body::from(input.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        if route == "body" {
            assert_eq!(body["text"], text);
        }
        if route == "query" {
            assert_eq!(body["entries"], json!([]));
        }
        let response = server
            .clone()
            .oneshot(
                request(&format!("/v1/fs/unknown/seq/{route}"))
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("x-operation-id", "forbidden")
                    .body(Body::from(input.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let readonly = app_export(Export::open(root.path()).unwrap());
    let response = readonly
        .oneshot(
            request("/v1/fs/docs/seq/body/put")
                .method("POST")
                .header("content-type", "application/json")
                .header("x-operation-id", "readonly-body")
                .body(Body::from(
                    json!({"path":"info.seq","sha256":hash,"text":text}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
