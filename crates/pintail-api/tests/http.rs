use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    hash::{Hash as _, Hasher as _},
};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use pintail_probe::{
    ProbeReport, RecommendedMode, ServerIdentity, SourceCapabilities, SourceColumn, SourceFlavor,
    SourceKey, SourceTable,
};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{DataType, KeyMode, KeyPart, PrimaryKey, StoredRow, Value as PintailValue};

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

fn test_source_table() -> SourceTable {
    SourceTable {
        name: "events".to_owned(),
        engine: Some("InnoDB".to_owned()),
        estimated_rows: Some(2),
        columns: vec![
            SourceColumn {
                id: 1,
                name: "id".to_owned(),
                mysql_data_type: "bigint".to_owned(),
                mysql_column_type: "bigint unsigned".to_owned(),
                pintail_type: DataType::UInt64,
                nullable: false,
                character_set: None,
                collation: None,
                generated_stored: false,
                auto_increment: true,
            },
            SourceColumn {
                id: 2,
                name: "name".to_owned(),
                mysql_data_type: "varchar".to_owned(),
                mysql_column_type: "varchar(255)".to_owned(),
                pintail_type: DataType::Utf8,
                nullable: true,
                character_set: Some("utf8mb4".to_owned()),
                collation: Some("utf8mb4_0900_ai_ci".to_owned()),
                generated_stored: false,
                auto_increment: false,
            },
        ],
        key: SourceKey {
            mode: KeyMode::Primary,
            index_name: Some("PRIMARY".to_owned()),
            columns: vec!["id".to_owned()],
        },
        unique_keys: Vec::new(),
        requires_reconciliation: false,
        warnings: Vec::new(),
    }
}

fn test_probe_report(database_name: &str, source: SourceTable) -> ProbeReport {
    ProbeReport {
        database: database_name.to_owned(),
        server: ServerIdentity {
            version: "8.4.0".to_owned(),
            version_comment: "MySQL Community Server".to_owned(),
            flavor: SourceFlavor::Mysql,
        },
        variables: BTreeMap::new(),
        grants: Vec::new(),
        capabilities: SourceCapabilities {
            log_bin: true,
            row_binlog: true,
            full_row_image: true,
            full_row_metadata: true,
            replication_grants: true,
            global_read_lock: true,
            gtid_available: true,
            recommended_mode: RecommendedMode::Cdc,
            reasons: Vec::new(),
        },
        tables: vec![source],
        warnings: Vec::new(),
    }
}

fn seed_mirrored_table(data_dir: &std::path::Path, database_id: &str, database_name: &str) {
    let source = test_source_table();
    let report = test_probe_report(database_name, source.clone());
    let mut metadata = pintail_meta::MetaStore::open(&data_dir.join("pintail-meta.db")).unwrap();
    metadata
        .update_database_probe(
            database_id,
            &serde_json::to_string(&report).unwrap(),
            "cdc",
            "2026-07-30T00:00:00Z",
        )
        .unwrap();
    metadata
        .upsert_snapshot_table(database_id, "events", Some(r#"["id"]"#), Some(r#"["id"]"#))
        .unwrap();
    metadata
        .start_snapshot_chunk(database_id, "events", "all", None, None)
        .unwrap();
    metadata
        .complete_snapshot_chunk(database_id, "events", "all", 2)
        .unwrap();
    metadata
        .set_database_replication_state(database_id, "cdc", "2026-07-30T00:00:01Z")
        .unwrap();

    let table_root = data_dir.join("databases").join(database_id).join("tables");
    let safe = "events";
    let mut hasher = DefaultHasher::new();
    safe.hash(&mut hasher);
    let directory = table_root.join(format!("table-{safe}-{:016x}", hasher.finish()));
    let mut store = TableStore::open(
        directory,
        source.table_schema().unwrap(),
        StoreOptions::default(),
    )
    .unwrap();
    store
        .ingest(vec![
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(1)]).unwrap(),
                vec![
                    PintailValue::UInt64(1),
                    PintailValue::Utf8("launch".to_owned()),
                ],
                1,
                false,
            ),
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(2)]).unwrap(),
                vec![
                    PintailValue::UInt64(2),
                    PintailValue::Utf8("land".to_owned()),
                ],
                2,
                false,
            ),
        ])
        .unwrap();
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
    assert!(html.contains("Opening control plane"));
    assert!(html.contains("Pintail turns live MySQL data into fast columnar analytics."));
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
async fn table_controls_require_authentication_and_a_known_table() {
    let data = tempfile::tempdir().expect("API data directory");
    let app = pintail_api::router_with_state(configured_state(data.path()));
    let authorization = format!("Bearer {}", setup_admin(&app).await);
    let database = create_database(&app, &authorization, "app").await;
    let database_id = database["id"].as_str().expect("database ID");

    for action in ["resync", "reconcile"] {
        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/databases/{database_id}/tables/events/{action}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/databases/{database_id}/tables/events/{action}"
                    ))
                    .header(header::AUTHORIZATION, &authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_response(missing).await,
            serde_json::json!({"error": "table does not exist"})
        );
    }
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
    let metadata = pintail_meta::MetaStore::open(&data.path().join("pintail-meta.db")).unwrap();
    metadata
        .start_sync_run(
            "run-first",
            first_id,
            Some("events"),
            "snapshot",
            "2026-07-30T00:00:00Z",
        )
        .unwrap();
    metadata
        .start_sync_run(
            "run-second",
            second_id,
            Some("users"),
            "snapshot",
            "2026-07-30T00:00:01Z",
        )
        .unwrap();

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

    let scoped_activity = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/activity")
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let scoped_activity = json_response(scoped_activity).await;
    assert_eq!(scoped_activity.as_array().unwrap().len(), 1);
    assert_eq!(scoped_activity[0]["database_id"], first_id);

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

#[tokio::test]
async fn query_and_table_routes_read_the_same_mirrored_snapshot() {
    let data = tempfile::tempdir().expect("API data directory");
    let app = pintail_api::router_with_state(configured_state(data.path()));
    let authorization = format!("Bearer {}", setup_admin(&app).await);
    let database = create_database(&app, &authorization, "analytics").await;
    let database_id = database["id"].as_str().expect("database ID");
    seed_mirrored_table(data.path(), database_id, "analytics");

    let query = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/query")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, &authorization)
                .body(Body::from(format!(
                    r#"{{"db":"{database_id}","sql":"SELECT id, name FROM events ORDER BY id"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(query.status(), StatusCode::OK);
    let query = json_response(query).await;
    assert_eq!(
        query["rows"],
        serde_json::json!([[1, "launch"], [2, "land"]])
    );
    assert_eq!(query["stats"]["rows"], 2);

    let write = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/query")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, &authorization)
                .body(Body::from(format!(
                    r#"{{"db":"{database_id}","sql":"DELETE FROM events"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(write.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_response(write).await["error"],
        "Pintail's HTTP query surface is read-only"
    );

    let schema = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/tables/events/schema?db={database_id}"))
                .header(header::AUTHORIZATION, &authorization)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(schema.status(), StatusCode::OK);
    let schema = json_response(schema).await;
    assert_eq!(schema["key_columns"], serde_json::json!(["id"]));
    assert_eq!(schema["columns"][1]["name"], "name");

    let count = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/tables/events/count?db={database_id}"))
                .header(header::AUTHORIZATION, authorization)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_response(count).await, serde_json::json!({"count": 2}));
}
