use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::hash::{Hash as _, Hasher as _};
use std::process::Command;

use mysql_async::{Opts, Pool, prelude::Queryable as _};
use pintail_meta::{MetaStore, NewApiKey};
use pintail_probe::{
    ProbeReport, RecommendedMode, ServerIdentity, SourceCapabilities, SourceColumn, SourceFlavor,
    SourceKey, SourceTable,
};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{DataType, KeyMode, KeyPart, PrimaryKey, StoredRow, Value};
use sha1::{Digest as _, Sha1};
use sha2::{Digest as _, Sha256};
use tokio::{net::TcpListener, sync::oneshot};

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn mysql_client_auth_metadata_prepared_query_and_read_only_error() {
    let data = tempfile::tempdir().expect("wire data directory");
    let metadata_path = data.path().join("pintail-meta.db");
    seed_replica(data.path(), &metadata_path);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("wire listener");
    let address = listener.local_addr().expect("wire address");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let data_dir = data.path().to_path_buf();
    let server_metadata = metadata_path.clone();
    let server = tokio::spawn(async move {
        pintail_wire::serve_until(listener, data_dir, server_metadata, async {
            let _ = shutdown_rx.await;
        })
        .await
    });

    let dsn = format!("mysql://analytics:pk_wire_secret@{address}/analytics");
    let pool = Pool::new(Opts::from_url(&dsn).expect("wire DSN"));
    let mut connection = pool.get_conn().await.expect("authenticated wire client");
    connection
        .query_drop("SET NAMES utf8mb4")
        .await
        .expect("session setup");
    let rows: Vec<(u64, String)> = connection
        .query("SELECT id, name FROM events ORDER BY id")
        .await
        .expect("wire query");
    assert_eq!(rows, vec![(1, "launch".to_owned()), (2, "land".to_owned())]);

    let prepared: Vec<(u64, String)> = connection
        .exec("SELECT id, name FROM events WHERE id = ?", (2_u64,))
        .await
        .expect("prepared wire query");
    assert_eq!(prepared, vec![(2, "land".to_owned())]);
    let fidelity = connection
        .exec_first::<mysql_async::Row, _, _>(
            "SELECT decimal_exact, date_value, datetime_value, time_value, \
                    json_value, text_value, binary_value, bool_value, signed_value \
             FROM type_fidelity WHERE id = ?",
            (1_u64,),
        )
        .await
        .expect("prepared type-fidelity query")
        .expect("type-fidelity row")
        .unwrap();
    assert_eq!(
        fidelity,
        vec![
            mysql_async::Value::Bytes(b"1234567890123456789012345678.1234567890".to_vec()),
            mysql_async::Value::Date(1000, 1, 1, 0, 0, 0, 0),
            mysql_async::Value::Date(2024, 2, 29, 12, 34, 56, 123_456),
            mysql_async::Value::Time(true, 2, 3, 4, 5, 600_000),
            mysql_async::Value::Bytes(br#"{"a":1,"b":[true,null]}"#.to_vec()),
            mysql_async::Value::Bytes("café βeta red,blue 🪿".as_bytes().to_vec()),
            mysql_async::Value::Bytes(vec![0, 255, 16, 222, 173, 190, 239]),
            mysql_async::Value::Int(1),
            mysql_async::Value::Int(-128),
        ]
    );
    let normalized_zero_dates = connection
        .exec_first::<(Option<String>, Option<String>), _, _>(
            "SELECT date_value, datetime_value FROM type_fidelity WHERE id = ?",
            (2_u64,),
        )
        .await
        .expect("normalized zero-date query");
    assert_eq!(normalized_zero_dates, Some((None, None)));

    let tables: Vec<String> = connection.query("SHOW TABLES").await.expect("SHOW TABLES");
    assert_eq!(tables, vec!["events", "type_fidelity"]);
    let description: Vec<mysql_async::Row> =
        connection.query("DESCRIBE events").await.expect("DESCRIBE");
    assert_eq!(description.len(), 2);
    let columns: Vec<(String, String, u64)> = connection
        .query(
            "SELECT table_name, column_name, ordinal_position \
             FROM information_schema.columns \
             WHERE table_schema = 'analytics' AND table_name = 'events' \
             ORDER BY ordinal_position",
        )
        .await
        .expect("information_schema.columns");
    assert_eq!(
        columns,
        vec![
            ("events".to_owned(), "id".to_owned(), 1),
            ("events".to_owned(), "name".to_owned(), 2)
        ]
    );
    let total: Option<u64> = connection
        .query_first("SELECT COUNT(*) AS total FROM events")
        .await
        .expect("BI aggregate");
    assert_eq!(total, Some(2));
    let explain: Vec<String> = connection
        .query("EXPLAIN SELECT id FROM events WHERE id = 2")
        .await
        .expect("EXPLAIN");
    assert_eq!(explain.len(), 1);
    assert!(explain[0].contains("Scan"));

    let error = connection
        .query_drop("DELETE FROM events")
        .await
        .expect_err("writes must fail");
    assert!(error.to_string().contains("read-only"), "{error}");
    drop(connection);
    pool.disconnect().await.expect("disconnect wire client");

    let wrong = Pool::new(
        Opts::from_url(&format!("mysql://analytics:wrong@{address}/analytics"))
            .expect("wrong-key DSN"),
    );
    assert!(wrong.get_conn().await.is_err());
    wrong.disconnect().await.expect("disconnect rejected pool");

    if std::env::var_os("PINTAIL_EXTERNAL_WIRE_CLIENTS").is_some() {
        external_client_gate(address);
    }

    let _ = shutdown_tx.send(());
    server
        .await
        .expect("wire server task")
        .expect("wire server");
}

fn external_client_gate(address: std::net::SocketAddr) {
    let port = address.port().to_string();
    let mysql_cli = std::env::var_os("PINTAIL_MYSQL_CLI").unwrap_or_else(|| "mysql".into());
    let cli = Command::new(&mysql_cli)
        .args([
            "--protocol=tcp",
            "--host",
            "127.0.0.1",
            "--port",
            &port,
            "--user",
            "analytics",
            "--database",
            "analytics",
            "--batch",
            "--skip-column-names",
            "--execute",
            "SELECT id, name FROM events ORDER BY id",
        ])
        .env("MYSQL_PWD", "pk_wire_secret")
        .output()
        .unwrap_or_else(|error| panic!("run mysql CLI {mysql_cli:?}: {error}"));
    assert!(
        cli.status.success(),
        "mysql CLI failed: {}",
        String::from_utf8_lossy(&cli.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&cli.stdout).trim(),
        "1\tlaunch\n2\tland"
    );

    let clients = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/integration/wire-clients");
    let mysql2 = Command::new("bun")
        .args(["run", "client.ts"])
        .current_dir(&clients)
        .env("PINTAIL_WIRE_HOST", "127.0.0.1")
        .env("PINTAIL_WIRE_PORT", &port)
        .output()
        .expect("run mysql2 client with Bun");
    assert!(
        mysql2.status.success(),
        "mysql2 failed: {}",
        String::from_utf8_lossy(&mysql2.stderr)
    );
    assert!(
        String::from_utf8_lossy(&mysql2.stdout).contains(r#""name":"land""#),
        "{}",
        String::from_utf8_lossy(&mysql2.stdout)
    );
    assert!(
        String::from_utf8_lossy(&mysql2.stdout).contains(r#""COLUMN_NAME":"name""#),
        "{}",
        String::from_utf8_lossy(&mysql2.stdout)
    );

    let pymysql = Command::new("uv")
        .args(["run", "--with", "pymysql", "python", "client.py"])
        .current_dir(clients)
        .env("PINTAIL_WIRE_HOST", "127.0.0.1")
        .env("PINTAIL_WIRE_PORT", port)
        .output()
        .expect("run PyMySQL client with uv");
    assert!(
        pymysql.status.success(),
        "PyMySQL failed: {}",
        String::from_utf8_lossy(&pymysql.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&pymysql.stdout).trim(),
        r"[2, 1, 2]"
    );
}

#[allow(clippy::too_many_lines)]
fn seed_replica(data_dir: &std::path::Path, metadata_path: &std::path::Path) {
    let source = source_table();
    let type_source = type_table();
    let report = probe_report(vec![source.clone(), type_source.clone()]);
    let mut metadata = MetaStore::open(metadata_path).expect("metadata");
    metadata
        .upsert_database("db-1", "analytics", b"unused", "2026-07-30T00:00:00Z")
        .unwrap();
    metadata
        .update_database_probe(
            "db-1",
            &serde_json::to_string(&report).unwrap(),
            "polling",
            "2026-07-30T00:00:01Z",
        )
        .unwrap();
    metadata
        .upsert_snapshot_table("db-1", "events", Some(r#"["id"]"#), Some(r#"["id"]"#))
        .unwrap();
    metadata
        .start_snapshot_chunk("db-1", "events", "all", None, None)
        .unwrap();
    metadata
        .complete_snapshot_chunk("db-1", "events", "all", 2)
        .unwrap();
    metadata
        .upsert_snapshot_table(
            "db-1",
            "type_fidelity",
            Some(r#"["id"]"#),
            Some(r#"["id"]"#),
        )
        .unwrap();
    metadata
        .start_snapshot_chunk("db-1", "type_fidelity", "all", None, None)
        .unwrap();
    metadata
        .complete_snapshot_chunk("db-1", "type_fidelity", "all", 2)
        .unwrap();
    metadata
        .set_database_replication_state("db-1", "polling", "2026-07-30T00:00:02Z")
        .unwrap();

    let secret = b"pk_wire_secret";
    let sha256 = Sha256::digest(secret);
    let native = Sha1::digest(Sha1::digest(secret));
    metadata
        .create_api_key(&NewApiKey {
            id: "key-wire",
            database_id: "db-1",
            name: "wire gate",
            sha256: &sha256,
            mysql_native_password_hash: Some(&native),
            scopes_json: r#"["query","read"]"#,
            expires_at: None,
            now: "2026-07-30T00:00:03Z",
        })
        .unwrap();

    let root = data_dir.join("databases").join("db-1").join("tables");
    let mut hasher = DefaultHasher::new();
    "events".hash(&mut hasher);
    let directory = root.join(format!("table-events-{:016x}", hasher.finish()));
    assert_eq!(directory, pintail_wire::table_directory(&root, "events"));
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
                vec![Value::UInt64(1), Value::Utf8("launch".to_owned())],
                1,
                false,
            ),
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(2)]).unwrap(),
                vec![Value::UInt64(2), Value::Utf8("land".to_owned())],
                2,
                false,
            ),
        ])
        .unwrap();

    let mut type_store = TableStore::open(
        pintail_wire::table_directory(&root, "type_fidelity"),
        type_source.table_schema().unwrap(),
        StoreOptions::default(),
    )
    .unwrap();
    type_store
        .ingest(vec![
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(1)]).unwrap(),
                vec![
                    Value::UInt64(1),
                    Value::Utf8("1234567890123456789012345678.1234567890".to_owned()),
                    Value::Utf8("1000-01-01".to_owned()),
                    Value::Utf8("2024-02-29 12:34:56.123456".to_owned()),
                    Value::Utf8("-51:04:05.600000".to_owned()),
                    Value::Utf8(r#"{"a":1,"b":[true,null]}"#.to_owned()),
                    Value::Utf8("café βeta red,blue 🪿".to_owned()),
                    Value::Binary(vec![0, 255, 16, 222, 173, 190, 239]),
                    Value::Boolean(true),
                    Value::Int64(-128),
                ],
                1,
                false,
            ),
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(2)]).unwrap(),
                vec![
                    Value::UInt64(2),
                    Value::Utf8("0.0000000000".to_owned()),
                    Value::Null,
                    Value::Null,
                    Value::Utf8("00:00:00.000000".to_owned()),
                    Value::Utf8("{}".to_owned()),
                    Value::Utf8(String::new()),
                    Value::Binary(Vec::new()),
                    Value::Boolean(false),
                    Value::Int64(0),
                ],
                2,
                false,
            ),
        ])
        .unwrap();
}

fn source_table() -> SourceTable {
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

fn type_table() -> SourceTable {
    let definitions = [
        (
            "decimal_exact",
            "decimal",
            "decimal(38,10)",
            DataType::Decimal {
                precision: 38,
                scale: 10,
            },
            true,
        ),
        ("date_value", "date", "date", DataType::Date32, true),
        (
            "datetime_value",
            "datetime",
            "datetime(6)",
            DataType::DateTime64 { fsp: 6 },
            true,
        ),
        (
            "time_value",
            "time",
            "time(6)",
            DataType::Time64 { fsp: 6 },
            true,
        ),
        ("json_value", "json", "json", DataType::Json, true),
        (
            "text_value",
            "varchar",
            "varchar(255)",
            DataType::Utf8,
            true,
        ),
        ("binary_value", "blob", "blob", DataType::Binary, true),
        (
            "bool_value",
            "tinyint",
            "tinyint(1)",
            DataType::Boolean,
            true,
        ),
        ("signed_value", "tinyint", "tinyint", DataType::Int8, true),
    ];
    let mut columns = vec![SourceColumn {
        id: 1,
        name: "id".to_owned(),
        mysql_data_type: "bigint".to_owned(),
        mysql_column_type: "bigint unsigned".to_owned(),
        pintail_type: DataType::UInt64,
        nullable: false,
        character_set: None,
        collation: None,
        generated_stored: false,
        auto_increment: false,
    }];
    columns.extend(definitions.into_iter().enumerate().map(
        |(index, (name, mysql_data_type, mysql_column_type, pintail_type, nullable))| {
            SourceColumn {
                id: u32::try_from(index + 2).unwrap(),
                name: name.to_owned(),
                mysql_data_type: mysql_data_type.to_owned(),
                mysql_column_type: mysql_column_type.to_owned(),
                pintail_type,
                nullable,
                character_set: matches!(pintail_type, DataType::Utf8 | DataType::Json)
                    .then(|| "utf8mb4".to_owned()),
                collation: matches!(pintail_type, DataType::Utf8)
                    .then(|| "utf8mb4_0900_ai_ci".to_owned()),
                generated_stored: false,
                auto_increment: false,
            }
        },
    ));
    SourceTable {
        name: "type_fidelity".to_owned(),
        engine: Some("InnoDB".to_owned()),
        estimated_rows: Some(2),
        columns,
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

fn probe_report(tables: Vec<SourceTable>) -> ProbeReport {
    ProbeReport {
        database: "analytics".to_owned(),
        server: ServerIdentity {
            version: "8.4.0".to_owned(),
            version_comment: "MySQL Community Server".to_owned(),
            flavor: SourceFlavor::Mysql,
        },
        variables: BTreeMap::new(),
        grants: Vec::new(),
        capabilities: SourceCapabilities {
            log_bin: false,
            row_binlog: false,
            full_row_image: false,
            full_row_metadata: false,
            replication_grants: false,
            global_read_lock: true,
            gtid_available: false,
            recommended_mode: RecommendedMode::Polling,
            reasons: Vec::new(),
        },
        tables,
        warnings: Vec::new(),
    }
}
