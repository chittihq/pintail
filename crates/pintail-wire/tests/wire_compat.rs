use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::hash::{Hash as _, Hasher as _};
use std::process::Command;

use mysql_async::{
    Opts, Pool,
    consts::{ColumnFlags, ColumnType},
    prelude::Queryable as _,
};
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
async fn wire_tls_negotiates_and_required_tls_refuses_plaintext() {
    // mysql_async's rustls client hits the same multi-backend ambiguity the
    // server pins away internally; tests pick ring process-wide.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let data = tempfile::tempdir().expect("wire data directory");
    let metadata_path = data.path().join("pintail-meta.db");
    seed_replica(data.path(), &metadata_path);

    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .expect("self-signed certificate");
    let certificate_path = data.path().join("wire.crt");
    let key_path = data.path().join("wire.key");
    std::fs::write(&certificate_path, certified.cert.pem()).expect("write certificate");
    std::fs::write(&key_path, certified.key_pair.serialize_pem()).expect("write key");
    let tls =
        pintail_wire::load_wire_tls(&certificate_path, &key_path, true).expect("load TLS policy");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("wire listener");
    let address = listener.local_addr().expect("wire address");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let data_dir = data.path().to_path_buf();
    let server_metadata = metadata_path.clone();
    let server = tokio::spawn(async move {
        pintail_wire::serve_until_with_options(
            listener,
            data_dir,
            server_metadata,
            pintail_wire::DEFAULT_QUERY_MEMORY_LIMIT,
            Some(tls),
            async {
                let _ = shutdown_rx.await;
            },
        )
        .await
    });

    // TLS client succeeds end to end.
    let ssl = mysql_async::SslOpts::default()
        .with_danger_accept_invalid_certs(true)
        .with_danger_skip_domain_validation(true);
    let secure = mysql_async::OptsBuilder::from_opts(
        Opts::from_url(&format!(
            "mysql://analytics:pk_wire_secret@{address}/analytics"
        ))
        .expect("wire DSN"),
    )
    .ssl_opts(Some(ssl));
    let pool = Pool::new(secure);
    let mut connection = pool.get_conn().await.expect("TLS wire client");
    let rows: Vec<(u64, String)> = connection
        .query("SELECT id, name FROM events ORDER BY id")
        .await
        .expect("TLS wire query");
    assert_eq!(rows, vec![(1, "launch".to_owned()), (2, "land".to_owned())]);
    drop(connection);
    pool.disconnect().await.expect("pool shutdown");

    // Plaintext client is refused when TLS is required.
    let plain = Pool::new(
        Opts::from_url(&format!(
            "mysql://analytics:pk_wire_secret@{address}/analytics"
        ))
        .expect("wire DSN"),
    );
    assert!(
        plain.get_conn().await.is_err(),
        "plaintext connection must be refused when TLS is required"
    );
    plain.disconnect().await.ok();

    let _ = shutdown_tx.send(());
    let _ = server.await;
}

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
    // Real session state: time_zone shifts statement-pinned NOW() and
    // echoes through @@session probes; bad zones and charsets error.
    connection
        .query_drop("SET time_zone = '+05:30'")
        .await
        .expect("set time zone");
    let echoed: Option<String> = connection
        .query_first("SELECT @@session.time_zone")
        .await
        .expect("time zone probe");
    assert_eq!(echoed.as_deref(), Some("+05:30"));
    let ahead: Option<String> = connection
        .query_first("SELECT NOW()")
        .await
        .expect("now ahead");
    connection
        .query_drop("SET time_zone = '-02:00'")
        .await
        .expect("set second zone");
    let behind: Option<String> = connection
        .query_first("SELECT NOW()")
        .await
        .expect("now behind");
    let parse = |text: &str| {
        chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").expect("NOW format")
    };
    let offset = parse(&ahead.expect("ahead value"))
        .signed_duration_since(parse(&behind.expect("behind value")))
        .num_seconds();
    let expected = i64::from(5 * 3600 + 30 * 60 + 2 * 3600);
    assert!(
        (offset - expected).abs() <= 5,
        "NOW() difference {offset}s should track the 7.5h zone gap"
    );
    assert!(
        connection
            .query_drop("SET time_zone = 'Bad/Zone'")
            .await
            .is_err(),
        "unknown time zones must error"
    );
    assert!(
        connection.query_drop("SET NAMES latin1").await.is_err(),
        "unsupported charsets must error"
    );
    connection
        .query_drop("SET NAMES utf8mb3")
        .await
        .expect("set utf8mb3 names");
    let mut utf8mb3_result = connection
        .query_iter("SELECT name FROM events LIMIT 1")
        .await
        .expect("utf8mb3 result metadata");
    assert_eq!(
        utf8mb3_result.columns().expect("utf8mb3 columns")[0].character_set(),
        33
    );
    let _: Vec<mysql_async::Row> = utf8mb3_result.collect().await.expect("utf8mb3 rows");
    connection
        .query_drop("SET SESSION character_set_results = 'binary'")
        .await
        .expect("set binary result charset");
    let mut binary_result = connection
        .query_iter("SELECT name FROM events LIMIT 1")
        .await
        .expect("binary result metadata");
    let binary_column = binary_result.columns().expect("binary columns")[0].clone();
    assert_eq!(binary_column.character_set(), 63);
    assert!(binary_column.flags().contains(ColumnFlags::BINARY_FLAG));
    let _: Vec<mysql_async::Row> = binary_result.collect().await.expect("binary rows");
    connection
        .query_drop("SET NAMES utf8mb4")
        .await
        .expect("restore utf8mb4 names");
    connection
        .query_drop("SET sql_mode = 'ANSI_QUOTES'")
        .await
        .expect("set sql mode");
    let mode: Option<String> = connection
        .query_first("SELECT @@sql_mode")
        .await
        .expect("sql mode probe");
    assert_eq!(mode.as_deref(), Some("ANSI_QUOTES"));
    connection
        .query_drop("SET SESSION group_concat_max_len = 5")
        .await
        .expect("set group concat limit");
    let concat_limit: Option<u64> = connection
        .query_first("SELECT @@group_concat_max_len")
        .await
        .expect("group concat limit probe");
    assert_eq!(concat_limit, Some(5));
    let mut short_concat_metadata = connection
        .query_iter("SELECT GROUP_CONCAT('x') FROM events")
        .await
        .expect("short group concat metadata");
    assert_eq!(
        short_concat_metadata
            .columns()
            .expect("short group concat columns")[0]
            .column_type(),
        ColumnType::MYSQL_TYPE_VAR_STRING
    );
    let _: Vec<mysql_async::Row> = short_concat_metadata
        .collect()
        .await
        .expect("short group concat rows");
    let truncated_concat: Option<String> = connection
        .query_first("SELECT GROUP_CONCAT(name ORDER BY id SEPARATOR '') FROM events")
        .await
        .expect("truncated group concat");
    assert_eq!(truncated_concat.as_deref(), Some("launc"));
    let warnings: Vec<(String, u64, String)> = connection
        .query("SHOW WARNINGS")
        .await
        .expect("group concat warnings");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].0, "Warning");
    assert_eq!(warnings[0].1, 1260);
    assert!(warnings[0].2.contains("GROUP_CONCAT"));
    connection
        .query_drop("SET SESSION group_concat_max_len = 1024")
        .await
        .expect("restore group concat limit");
    let mut long_concat_metadata = connection
        .query_iter("SELECT GROUP_CONCAT('x') FROM events")
        .await
        .expect("long group concat metadata");
    assert_eq!(
        long_concat_metadata
            .columns()
            .expect("long group concat columns")[0]
            .column_type(),
        ColumnType::MYSQL_TYPE_BLOB
    );
    let _: Vec<mysql_async::Row> = long_concat_metadata
        .collect()
        .await
        .expect("long group concat rows");
    connection
        .query_drop("SET time_zone = 'SYSTEM'")
        .await
        .expect("restore zone");
    let rows: Vec<(u64, String)> = connection
        .query("SELECT id, name FROM events ORDER BY id")
        .await
        .expect("wire query");
    assert_eq!(rows, vec![(1, "launch".to_owned()), (2, "land".to_owned())]);

    let mut json_result = connection
        .query_iter(
            "SELECT JSON_OBJECT('json', JSON_EXTRACT('{\"x\":1}', '$'), \
                                'text', '{\"x\":1}'), \
                    JSON_EXTRACT('null', '$'), JSON_EXTRACT(NULL, '$')",
        )
        .await
        .expect("text-protocol JSON query");
    let json_columns = json_result.columns().expect("JSON result metadata");
    assert!(
        json_columns
            .iter()
            .all(|column| column.column_type() == ColumnType::MYSQL_TYPE_JSON)
    );
    let json_rows: Vec<mysql_async::Row> = json_result
        .collect()
        .await
        .expect("text-protocol JSON rows");
    assert_eq!(
        json_rows.into_iter().next().expect("JSON row").unwrap(),
        vec![
            mysql_async::Value::Bytes(br#"{"json": {"x": 1}, "text": "{\"x\":1}"}"#.to_vec(),),
            mysql_async::Value::Bytes(b"null".to_vec()),
            mysql_async::Value::NULL,
        ]
    );

    let mut year_result = connection
        .query_iter("SELECT CAST(69 AS YEAR)")
        .await
        .expect("YEAR cast query");
    assert_eq!(
        year_result.columns().expect("YEAR result metadata")[0].column_type(),
        ColumnType::MYSQL_TYPE_YEAR
    );
    let year_rows: Vec<mysql_async::Row> = year_result.collect().await.expect("YEAR rows");
    assert_eq!(
        year_rows.into_iter().next().expect("YEAR row").unwrap(),
        vec![mysql_async::Value::Bytes(b"2069".to_vec())]
    );

    let json_statement = connection
        .prep(
            "SELECT JSON_OBJECT('json', json_value, 'text', text_value), \
                    JSON_ARRAY(json_value, text_value) \
             FROM type_fidelity WHERE id = ?",
        )
        .await
        .expect("prepare JSON query");
    assert!(
        json_statement
            .columns()
            .iter()
            .all(|column| column.column_type() == ColumnType::MYSQL_TYPE_JSON)
    );
    let prepared_json = connection
        .exec_first::<mysql_async::Row, _, _>(&json_statement, (1_u64,))
        .await
        .expect("prepared JSON query")
        .expect("prepared JSON row")
        .unwrap();
    assert_eq!(
        prepared_json,
        vec![
            mysql_async::Value::Bytes(
                "{\"json\": {\"a\": 1, \"b\": [true, null]}, \
                 \"text\": \"café βeta red,blue 🪿\"}"
                    .as_bytes()
                    .to_vec(),
            ),
            mysql_async::Value::Bytes(
                "[{\"a\": 1, \"b\": [true, null]}, \"café βeta red,blue 🪿\"]"
                    .as_bytes()
                    .to_vec(),
            ),
        ]
    );

    let prepared: Vec<(u64, String)> = connection
        .exec("SELECT id, name FROM events WHERE id = ?", (2_u64,))
        .await
        .expect("prepared wire query");
    assert_eq!(prepared, vec![(2, "land".to_owned())]);
    let by_date: Vec<u64> = connection
        .exec(
            "SELECT id FROM type_fidelity WHERE date_value = ?",
            (mysql_async::Value::Date(1000, 1, 1, 0, 0, 0, 0),),
        )
        .await
        .expect("prepared DATE parameter");
    assert_eq!(by_date, vec![1]);
    let by_datetime: Vec<u64> = connection
        .exec(
            "SELECT id FROM type_fidelity WHERE datetime_value = ?",
            (mysql_async::Value::Date(2024, 2, 29, 12, 34, 56, 123_456),),
        )
        .await
        .expect("prepared DATETIME parameter");
    assert_eq!(by_datetime, vec![1]);
    let by_time: Vec<u64> = connection
        .exec(
            "SELECT id FROM type_fidelity WHERE time_value = ?",
            (mysql_async::Value::Time(true, 2, 3, 4, 5, 600_000),),
        )
        .await
        .expect("prepared TIME parameter");
    assert_eq!(by_time, vec![1]);
    let fidelity_statement = connection
        .prep(
            "SELECT decimal_exact, date_value, datetime_value, time_value, \
                    json_value, text_value, binary_value, bool_value, signed_value \
             FROM type_fidelity WHERE id = ?",
        )
        .await
        .expect("prepare type-fidelity query");
    let fidelity_columns = fidelity_statement.columns();
    assert_eq!(fidelity_columns[0].decimals(), 10);
    assert_eq!(fidelity_columns[2].decimals(), 6);
    assert_eq!(fidelity_columns[3].decimals(), 6);
    assert_eq!(fidelity_columns[4].character_set(), 63);
    assert_eq!(fidelity_columns[5].character_set(), 255);
    assert_eq!(fidelity_columns[6].character_set(), 63);
    let fidelity = connection
        .exec_first::<mysql_async::Row, _, _>(&fidelity_statement, (1_u64,))
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
    let full_tables: Vec<(String, String)> = connection
        .query("SHOW FULL TABLES LIKE 'events'")
        .await
        .expect("SHOW FULL TABLES");
    assert_eq!(
        full_tables,
        vec![("events".to_owned(), "BASE TABLE".to_owned())]
    );
    let description: Vec<mysql_async::Row> =
        connection.query("DESCRIBE events").await.expect("DESCRIBE");
    assert_eq!(description.len(), 2);
    let filtered_description: Vec<mysql_async::Row> = connection
        .query("SHOW COLUMNS FROM events LIKE 'na%'")
        .await
        .expect("SHOW COLUMNS LIKE");
    assert_eq!(filtered_description.len(), 1);
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
    let discovery: Vec<(String, String, u64)> = connection
        .query(
            "SELECT c.table_name, t.table_type, COUNT(c.column_name) AS column_count \
             FROM information_schema.columns AS c \
             JOIN information_schema.tables AS t \
               ON c.table_schema = t.table_schema AND c.table_name = t.table_name \
             WHERE c.table_schema = 'analytics' AND c.table_name = 'events' \
             GROUP BY c.table_name, t.table_type",
        )
        .await
        .expect("joined metadata discovery query");
    assert_eq!(
        discovery,
        vec![("events".to_owned(), "BASE TABLE".to_owned(), 2)]
    );
    let indexes: Vec<mysql_async::Row> = connection
        .query("SHOW KEYS FROM events")
        .await
        .expect("SHOW KEYS");
    assert_eq!(indexes.len(), 1);
    assert_eq!(
        indexes[0].get::<String, _>("Table").as_deref(),
        Some("events")
    );
    assert_eq!(indexes[0].get::<i64, _>("Non_unique"), Some(0));
    assert_eq!(
        indexes[0].get::<String, _>("Key_name").as_deref(),
        Some("PRIMARY")
    );
    assert_eq!(indexes[0].get::<u64, _>("Seq_in_index"), Some(1));
    assert_eq!(
        indexes[0].get::<String, _>("Column_name").as_deref(),
        Some("id")
    );
    let views: Option<u64> = connection
        .query_first(
            "SELECT COUNT(*) FROM information_schema.views \
             WHERE table_schema = 'analytics'",
        )
        .await
        .expect("information_schema.views");
    assert_eq!(views, Some(0));
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
    const METADATA_CORPUS: &str =
        include_str!("../../../tests/integration/wire-clients/metadata.sql");
    let port = address.port().to_string();
    let mysql_cli = std::env::var_os("PINTAIL_MYSQL_CLI").unwrap_or_else(|| "mysql".into());
    let cli_sql = format!("SELECT id, name FROM events ORDER BY id; {METADATA_CORPUS}");
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
            &cli_sql,
        ])
        .env("MYSQL_PWD", "pk_wire_secret")
        .output()
        .unwrap_or_else(|error| panic!("run mysql CLI {mysql_cli:?}: {error}"));
    assert!(
        cli.status.success(),
        "mysql CLI failed: {}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_output = String::from_utf8_lossy(&cli.stdout);
    assert!(cli_output.contains("1\tlaunch\n2\tland"), "{cli_output}");
    assert!(cli_output.contains("PRIMARY"), "{cli_output}");

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
    assert!(
        String::from_utf8_lossy(&mysql2.stdout).contains(r#""view_count":0"#),
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
    let pymysql_output = String::from_utf8_lossy(&pymysql.stdout);
    assert!(
        pymysql_output.contains(r#""aggregate": [2, 1, 2]"#),
        "{pymysql_output}"
    );
    assert!(pymysql_output.contains("PRIMARY"), "{pymysql_output}");
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
    let caching_sha2 = Sha256::digest(Sha256::digest(secret));
    metadata
        .create_api_key(&NewApiKey {
            id: "key-wire",
            database_id: "db-1",
            name: "wire gate",
            sha256: &sha256,
            mysql_native_password_hash: Some(&native),
            caching_sha2_password_hash: Some(&caching_sha2),
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
                default_value: None,
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
                default_value: None,
            },
        ],
        key: SourceKey {
            mode: KeyMode::Primary,
            index_name: Some("PRIMARY".to_owned()),
            columns: vec!["id".to_owned()],
        },
        unique_keys: Vec::new(),
        requires_reconciliation: false,
        foreign_keys: Vec::new(),
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
        default_value: None,
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
                default_value: None,
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
        foreign_keys: Vec::new(),
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
