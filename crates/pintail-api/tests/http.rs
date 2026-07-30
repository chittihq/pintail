use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

fn configured_state(data_dir: &std::path::Path) -> pintail_api::ApiState {
    pintail_api::ApiState::new(
        data_dir,
        data_dir.join("pintail-meta.db"),
        b"test-jwt-secret-with-enough-entropy",
        &"42".repeat(32),
    )
    .expect("configured API state")
}

async fn json_response(response: axum::response::Response) -> Value {
    let body = response
        .into_body()
        .collect()
        .await
        .expect("JSON body")
        .to_bytes();
    serde_json::from_slice(&body).expect("valid JSON")
}

async fn setup_admin(app: &axum::Router) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/setup")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"email":"admin@example.com","password":"correct horse battery"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    json_response(response).await["token"]
        .as_str()
        .expect("setup token")
        .to_owned()
}

async fn create_database(app: &axum::Router, authorization: &str, name: &str) -> Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/databases")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, authorization)
                .body(Body::from(format!(
                    r#"{{"name":"{name}","dsn":"mysql://pintail:secret@db/{name}","mode":"auto"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    json_response(response).await
}

#[tokio::test]
async fn health_endpoint_reports_that_pintail_is_ready() {
    let response = pintail_api::router()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("health request"),
        )
        .await
        .expect("health response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE),
        Some(&header::HeaderValue::from_static("application/json"))
    );

    let body = response
        .into_body()
        .collect()
        .await
        .expect("health body")
        .to_bytes();
    let json: Value = serde_json::from_slice(&body).expect("health JSON");
    assert_eq!(json, serde_json::json!({ "status": "ok" }));
}

#[tokio::test]
async fn root_serves_the_embedded_dashboard() {
    let response = pintail_api::router()
        .oneshot(
            Request::builder()
                .uri("/")
                .body(Body::empty())
                .expect("dashboard request"),
        )
        .await
        .expect("dashboard response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE),
        Some(&header::HeaderValue::from_static(
            "text/html; charset=utf-8"
        ))
    );

    let body = response
        .into_body()
        .collect()
        .await
        .expect("dashboard body")
        .to_bytes();
    let html = String::from_utf8(body.to_vec()).expect("dashboard HTML");
    assert!(html.contains("<title>Pintail</title>"));
    assert!(html.contains("Columnar analytics"));
    assert!(html.contains("for MySQL."));
}

#[tokio::test]
async fn setup_login_and_protected_session_use_signed_tokens() {
    let data = tempfile::tempdir().expect("API data directory");
    let app = pintail_api::router_with_state(configured_state(data.path()));

    let status = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/setup/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        json_response(status).await,
        serde_json::json!({"required": true})
    );

    let setup = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/setup")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"email":"admin@example.com","password":"correct horse battery"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(setup.status(), StatusCode::OK);
    let setup_json = json_response(setup).await;
    let token = setup_json["token"].as_str().expect("setup JWT");

    let duplicate = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/setup")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"email":"other@example.com","password":"correct horse battery"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);

    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let session = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/session")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(session.status(), StatusCode::OK);
    assert_eq!(json_response(session).await["role"], "admin");

    let login = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"email":"ADMIN@example.com","password":"correct horse battery"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    assert!(json_response(login).await["token"].is_string());
}

#[tokio::test]
async fn database_crud_encrypts_dsn_and_requires_authentication() {
    let data = tempfile::tempdir().expect("API data directory");
    let state = configured_state(data.path());
    let app = pintail_api::router_with_state(state);
    let token = setup_admin(&app).await;
    let authorization = format!("Bearer {token}");

    let created = create_database(&app, &authorization, "app").await;
    assert!(created.get("dsn").is_none());
    let id = created["id"].as_str().expect("database ID");

    let metadata =
        pintail_meta::MetaStore::open(&data.path().join("pintail-meta.db")).expect("metadata");
    let stored = metadata.database(id).unwrap().expect("stored database");
    assert_ne!(
        stored.encrypted_dsn,
        b"mysql://pintail:secret@db/app".to_vec()
    );

    let listed = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/databases")
                .header(header::AUTHORIZATION, &authorization)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_response(listed).await.as_array().unwrap().len(), 1);

    let deleted = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/databases/{id}"))
                .header(header::AUTHORIZATION, authorization)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn api_key_secrets_are_shown_once_and_database_scoped() {
    let data = tempfile::tempdir().expect("API data directory");
    let app = pintail_api::router_with_state(configured_state(data.path()));
    let jwt = format!("Bearer {}", setup_admin(&app).await);
    let first = create_database(&app, &jwt, "first").await;
    let second = create_database(&app, &jwt, "second").await;
    let first_id = first["id"].as_str().unwrap();
    let second_id = second["id"].as_str().unwrap();

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/databases/{first_id}/api-keys"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, &jwt)
                .body(Body::from(
                    r#"{"name":"Metabase","scopes":["read","query"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = json_response(created).await;
    let secret = created["secret"].as_str().expect("one-time secret");
    assert!(secret.starts_with("pk_"));

    let listed = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/databases/{first_id}/api-keys"))
                .header(header::AUTHORIZATION, &jwt)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let listed = json_response(listed).await;
    assert!(listed[0].get("secret").is_none());

    let own_database = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/databases/{first_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(own_database.status(), StatusCode::OK);

    let other_database = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/databases/{second_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(other_database.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn snapshot_jobs_reject_paused_databases_without_leaking_a_job_slot() {
    let data = tempfile::tempdir().expect("API data directory");
    let app = pintail_api::router_with_state(configured_state(data.path()));
    let authorization = format!("Bearer {}", setup_admin(&app).await);
    let database = create_database(&app, &authorization, "paused_source").await;
    let database_id = database["id"].as_str().expect("database ID");

    let paused = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/databases/{database_id}/mode"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, &authorization)
                .body(Body::from(r#"{"mode":"paused"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/databases/{database_id}/snapshot"))
                    .header(header::AUTHORIZATION, &authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_response(response).await,
            serde_json::json!({
                "error": "resume the database before starting a snapshot"
            })
        );
    }
}
