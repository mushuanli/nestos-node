use crate::common::{app_export, body_json, mutate_request, request};
use axum::{body::Body, http::StatusCode};
use cells::fs::Export;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn metadata_uses_authenticated_mutations_and_normal_stat_reads() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note.md"), "Text").unwrap();
    let app = app_export(Export::exclusive(root.path()).unwrap());
    let response = app.clone().oneshot(mutate_request("meta", &json!({"action":"metadata","path":"note.md","metadata":{"_custom":{"anchor":"a"}},"removeKeys":[]}).to_string())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["outcome"], "committed");
    let response = app
        .clone()
        .oneshot(
            request("/v1/fs/docs/stat")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(json!({"paths":["note.md"]}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        body_json(response).await["results"][0]["stat"]["metadata"]["_custom"]["anchor"],
        "a"
    );
    let response = app
        .clone()
        .oneshot(mutate_request(
            "traversal",
            &json!({"action":"metadata","path":"../escape","metadata":{}}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(mutate_request(
            "private",
            &json!({"action":"metadata","path":".nestos/meta.sqlite","metadata":{}}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    drop(app);
    let readonly = app_export(Export::open(root.path()).unwrap());
    let response = readonly
        .oneshot(mutate_request(
            "denied",
            &json!({"action":"metadata","path":"note.md","metadata":{"x":1}}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn legacy_metadata_lists_readonly_and_migrates_identity_on_write() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note.md"), "Text").unwrap();
    let directory = root.path().join(".mindos");
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("meta.sqlite");
    let database = rusqlite::Connection::open(&path).unwrap();
    database.execute_batch("CREATE TABLE file_metadata(path TEXT PRIMARY KEY,data TEXT NOT NULL); INSERT INTO file_metadata VALUES('note.md','{\"title\":\"preserved\"}'); PRAGMA application_id=1296321876; PRAGMA user_version=1;").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let app = app_export(Export::exclusive(root.path()).unwrap());
    let response = app.clone().oneshot(request("/v1/fs/docs/entries?path=").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["entries"][0]["stat"]["metadata"]["title"], "preserved");
    assert!(database.prepare("SELECT identity FROM file_metadata").is_err());
    let response = app.clone().oneshot(mutate_request("upgrade", &json!({"action":"metadata","path":"note.md","metadata":{"extra":true}}).to_string())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let identity: String = database.query_row("SELECT identity FROM file_metadata", [], |row| row.get(0)).unwrap();
    assert_eq!(identity.len(), 32);
    let response = app.oneshot(mutate_request("again", &json!({"action":"metadata","path":"note.md","metadata":{"next":true}}).to_string())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(identity, database.query_row::<String,_,_>("SELECT identity FROM file_metadata", [], |row| row.get(0)).unwrap());
}
