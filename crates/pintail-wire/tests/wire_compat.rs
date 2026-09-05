// These tests take turns; see `wire_serial`. The guard is deliberately held
// across the awaits of a whole test, which is the point of it - an async-aware
// mutex would serialize the same way and read no better here, since nothing
// else in the process contends for it.
#![allow(clippy::await_holding_lock)]

use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::future::Future;
use std::hash::{Hash as _, Hasher as _};
use std::process::Command;
use std::time::Duration;

use mysql_async::{
    ChangeUserOpts, Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts,
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

/// Serializes the tests in this binary.
///
/// They share one process, and `a_saturated_server_refuses_queries_instead_of_
/// queueing_them` pins the process-wide admission bound to a single slot and
/// then holds that slot - so any sibling running beside it is refused with
/// "too many concurrent queries" and fails on a contract it never tested.
/// Run alone every test passes; run together one to three of five fail, and
/// which ones varies by scheduling.
///
/// The bound is a `OnceLock`, so it cannot be put back afterwards. Taking
/// turns is what is left, and it costs nothing measurable: the whole file
/// runs in about three seconds either way.
fn wire_serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // The data is `()`, so a panicking test leaves nothing to be corrupted
    // and the next one may proceed.
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[tokio::test(flavor = "multi_thread")]
async fn wire_tls_negotiates_and_required_tls_refuses_plaintext() {
    let _serial = wire_serial();
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
            pintail_wire::DEFAULT_WIRE_IDLE_TIMEOUT,
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
async fn wire_idle_timeout_closes_inactive_authenticated_connections() {
    let _serial = wire_serial();
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
        pintail_wire::serve_until_with_options(
            listener,
            data_dir,
            server_metadata,
            pintail_wire::DEFAULT_QUERY_MEMORY_LIMIT,
            None,
            Duration::from_millis(50),
            async {
                let _ = shutdown_rx.await;
            },
        )
        .await
    });

    let pool = Pool::new(
        Opts::from_url(&format!(
            "mysql://analytics:pk_wire_secret@{address}/analytics"
        ))
        .expect("wire DSN"),
    );
    let mut connection = pool.get_conn().await.expect("authenticated wire client");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        connection.ping().await.is_err(),
        "an inactive connection must be closed after its configured timeout"
    );
    drop(connection);
    pool.disconnect().await.ok();

    let _ = shutdown_tx.send(());
    server
        .await
        .expect("wire server task")
        .expect("wire server");
}

#[test]
fn disconnecting_clients_cancel_active_query_execution() {
    let _serial = wire_serial();
    let data = tempfile::tempdir().expect("wire data directory");
    let metadata_path = data.path().join("pintail-meta.db");
    seed_replica(data.path(), &metadata_path);
    append_cancellation_rows(data.path());

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let data_dir = data.path().to_path_buf();
    let server_metadata = metadata_path.clone();
    let server = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(2)
            .enable_all()
            .build()
            .expect("server runtime")
            .block_on(async move {
                let listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("wire listener");
                ready_tx
                    .send(listener.local_addr().expect("wire address"))
                    .expect("publish wire address");
                pintail_wire::serve_until(listener, data_dir, server_metadata, async move {
                    let _ = shutdown_rx.await;
                })
                .await
            })
    });
    let address = ready_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("wire server start");
    let dsn = format!("mysql://analytics:pk_wire_secret@{address}/analytics");
    let clients = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("client runtime");

    clients.block_on(async {
        let abandon = |dsn: String, prepared: bool| async move {
            let mut connection = mysql_async::Conn::new(Opts::from_url(&dsn).expect("wire DSN"))
                .await
                .expect("authenticated wire client");
            connection
                .query_drop("SET SESSION cte_max_recursion_depth = 1000000")
                .await
                .expect("raise recursion guard");
            let query: std::pin::Pin<
                Box<dyn Future<Output = Result<(), mysql_async::Error>> + Send + '_>,
            > = if prepared {
                // A recursive CTE keeps the workload active however fast the
                // scan gets: the previous events GROUP BY dropped under the
                // 250ms window once the engine's decode sped up, and this
                // test is about cancellation, not scan speed.
                let statement = connection
                    .prep(
                        "WITH RECURSIVE r (n) AS (SELECT 1 UNION ALL \
                         SELECT n + 1 FROM r WHERE n < ?) \
                         SELECT COUNT(*) FROM r",
                    )
                    .await
                    .expect("prepare cancellation workload");
                Box::pin(connection.exec_drop(statement, (1_000_000_u64,)))
            } else {
                Box::pin(connection.query_drop(
                    "WITH RECURSIVE r (n) AS (SELECT 1 UNION ALL \
                     SELECT n + 1 FROM r WHERE n < 1000000) \
                     SELECT COUNT(*) FROM r",
                ))
            };
            assert!(
                tokio::time::timeout(Duration::from_millis(250), query)
                    .await
                    .is_err(),
                "cancellation workload must still be active before disconnect"
            );
            drop(connection);
        };
        tokio::join!(abandon(dsn.clone(), false), abandon(dsn.clone(), true));

        let quick_query = tokio::time::timeout(Duration::from_secs(2), async {
            let mut connection =
                mysql_async::Conn::new(Opts::from_url(&dsn).expect("wire DSN")).await?;
            connection.query_first::<u64, _>("SELECT 1").await
        })
        .await;
        assert!(
            matches!(quick_query, Ok(Ok(Some(1)))),
            "abandoned execution must release server capacity promptly: {quick_query:?}"
        );
    });

    let _ = shutdown_tx.send(());
    server
        .join()
        .expect("wire server thread")
        .expect("wire server");
}

fn append_cancellation_rows(data_dir: &std::path::Path) {
    let root = data_dir.join("databases").join("db-1").join("tables");
    let mut store = TableStore::open(
        pintail_wire::table_directory(&root, "events"),
        source_table().table_schema().expect("events schema"),
        StoreOptions::default(),
    )
    .expect("events store");
    let rows = (3_u64..=20_002)
        .map(|id| {
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("event key"),
                vec![Value::UInt64(id), Value::Utf8(format!("cancel-{id:05}"))],
                id,
                false,
            )
        })
        .collect();
    store.ingest(rows).expect("cancellation rows");
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn mysql_client_auth_metadata_prepared_query_and_read_only_error() {
    let _serial = wire_serial();
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
    let pool = Pool::new(
        OptsBuilder::from_opts(Opts::from_url(&dsn).expect("wire DSN"))
            .pool_opts(PoolOpts::default().with_constraints(
                PoolConstraints::new(1, 1).expect("single-connection test pool"),
            )),
    );
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
    let name_column = &utf8mb3_result.columns().expect("utf8mb3 columns")[0];
    assert_eq!(name_column.character_set(), 33);
    assert!(
        name_column.flags().contains(ColumnFlags::NOT_NULL_FLAG),
        "direct result metadata must retain source NOT NULL for a non-key column"
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
    // ANSI_QUOTES changes how a statement parses, and the parser is a fixed
    // MySqlDialect. Accepting and echoing it - which this test used to
    // assert - meant a client could ask for identifier quoting, be told it
    // succeeded, and silently get string literals instead.
    let refused = connection
        .query_drop("SET sql_mode = 'ANSI_QUOTES'")
        .await
        .expect_err("a result-changing sql_mode must be refused");
    assert!(
        refused.to_string().contains("ANSI_QUOTES"),
        "refusal must name the mode, got: {refused}"
    );
    // A mode that is genuinely inert on a read-only replica still round-trips.
    connection
        .query_drop("SET sql_mode = 'STRICT_TRANS_TABLES'")
        .await
        .expect("an inert sql_mode must still be accepted");
    let mode: Option<String> = connection
        .query_first("SELECT @@sql_mode")
        .await
        .expect("sql mode probe");
    assert_eq!(mode.as_deref(), Some("STRICT_TRANS_TABLES"));
    connection
        .query_drop("SET SESSION group_concat_max_len = 5")
        .await
        .expect("set group concat limit");
    let concat_limit: Option<u64> = connection
        .query_first("SELECT @@group_concat_max_len")
        .await
        .expect("group concat limit probe");
    assert_eq!(concat_limit, Some(5));
    connection
        .query_drop("SET SESSION cte_max_recursion_depth = 12")
        .await
        .expect("set recursive CTE depth");
    let recursion_depth: Option<u64> = connection
        .query_first("SELECT @@cte_max_recursion_depth")
        .await
        .expect("recursive CTE depth probe");
    assert_eq!(recursion_depth, Some(12));
    assert!(
        connection
            .query_drop("SET SESSION cte_max_recursion_depth = 0")
            .await
            .is_err(),
        "an unbounded recursive CTE setting must reject"
    );
    connection
        .query_drop("SET SESSION max_execution_time = 1")
        .await
        .expect("set query deadline");
    connection
        .query_drop("SET SESSION cte_max_recursion_depth = 1000000")
        .await
        .expect("raise recursion guard above the timeout workload");
    let execution_time: Option<u64> = connection
        .query_first("SELECT @@max_execution_time")
        .await
        .expect("query deadline probe");
    assert_eq!(execution_time, Some(1));
    let timeout = connection
        .query_drop(
            "WITH RECURSIVE r (n) AS (\
             SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 1000000) \
             SELECT MAX(n) FROM r",
        )
        .await
        .expect_err("one millisecond deadline must interrupt recursive work");
    assert!(
        timeout.to_string().contains("1317") || timeout.to_string().contains("max_execution_time"),
        "unexpected timeout error: {timeout}"
    );
    connection
        .query_drop("SET SESSION max_execution_time = 0")
        .await
        .expect("disable query deadline");
    connection
        .query_drop("SET SESSION cte_max_recursion_depth = 1000")
        .await
        .expect("restore recursive CTE depth");
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
    let scalar_helpers = connection
        .exec_first::<mysql_async::Row, _, _>(
            "SELECT MD5(?), CONV(?, 16, 10), SUBSTRING_INDEX(?, '/', -1), \
                    IFNULL(?, 'fallback')",
            ("abc", "ff", "a/b/c", mysql_async::Value::NULL),
        )
        .await
        .expect("prepared scalar-helper query")
        .expect("prepared scalar-helper row")
        .unwrap();
    assert_eq!(
        scalar_helpers,
        vec![
            mysql_async::Value::Bytes(b"900150983cd24fb0d6963f7d28e17f72".to_vec()),
            mysql_async::Value::Bytes(b"255".to_vec()),
            mysql_async::Value::Bytes(b"c".to_vec()),
            mysql_async::Value::Bytes(b"fallback".to_vec()),
        ]
    );

    let cast_statement = connection
        .prep("SELECT CAST(? AS JSON), CAST(? AS YEAR), CAST(? AS TIME(3))")
        .await
        .expect("prepare cast result-family query");
    let cast_columns = cast_statement.columns();
    assert_eq!(cast_columns[0].column_type(), ColumnType::MYSQL_TYPE_JSON);
    assert_eq!(cast_columns[1].column_type(), ColumnType::MYSQL_TYPE_YEAR);
    assert_eq!(cast_columns[2].column_type(), ColumnType::MYSQL_TYPE_TIME);
    assert_eq!(cast_columns[2].decimals(), 3);
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
    let decimal_expression_statement = connection
        .prep(
            "SELECT CAST(12.34 AS DECIMAL(5,2)) + CAST(1.234 AS DECIMAL(4,3)), \
                    CAST(12.34 AS DECIMAL(5,2)) * CAST(1.234 AS DECIMAL(4,3)), \
                    CAST(12.34 AS DECIMAL(5,2)) / CAST(1.234 AS DECIMAL(4,3)), \
                    -CAST(12.34 AS DECIMAL(5,2))",
        )
        .await
        .expect("prepare decimal-expression query");
    let decimal_expression_columns = decimal_expression_statement.columns();
    assert_eq!(decimal_expression_columns.len(), 4);
    assert!(decimal_expression_columns.iter().all(|column| {
        column.column_type() == mysql_async::consts::ColumnType::MYSQL_TYPE_NEWDECIMAL
            && column.character_set() == 63
    }));
    assert_eq!(
        decimal_expression_columns
            .iter()
            .map(mysql_async::Column::decimals)
            .collect::<Vec<_>>(),
        vec![3, 5, 6, 2]
    );
    assert_eq!(
        decimal_expression_columns
            .iter()
            .map(mysql_async::Column::column_length)
            .collect::<Vec<_>>(),
        vec![9, 11, 14, 7]
    );
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

    // Pool-safe lifecycle commands preserve the physical connection while
    // discarding session and prepared-statement state.
    let connection_id = connection.id();
    let reset_statement = connection
        .prep("SELECT name FROM events WHERE id = ?")
        .await
        .expect("prepare before reset");
    connection
        .query_drop("SET time_zone = '+05:30'")
        .await
        .expect("dirty session before reset");
    connection
        .query_drop("SET max_execution_time = 1234")
        .await
        .expect("dirty query deadline before reset");
    assert!(connection.reset().await.expect("COM_RESET_CONNECTION"));
    assert_eq!(connection.id(), connection_id);
    let reset_zone: Option<String> = connection
        .query_first("SELECT @@session.time_zone")
        .await
        .expect("session after reset");
    assert_eq!(reset_zone.as_deref(), Some("SYSTEM"));
    let reset_execution_time: Option<u64> = connection
        .query_first("SELECT @@max_execution_time")
        .await
        .expect("query deadline after reset");
    assert_eq!(reset_execution_time, Some(0));
    assert!(
        connection
            .exec_drop(&reset_statement, (1_u64,))
            .await
            .is_err(),
        "connection reset must invalidate prepared statements"
    );

    let change_user_statement = connection
        .prep("SELECT name FROM events WHERE id = ?")
        .await
        .expect("prepare before change-user");
    // Any non-default mode dirties the session; the point here is that
    // COM_CHANGE_USER resets it, not which mode was set.
    connection
        .query_drop("SET sql_mode = 'STRICT_ALL_TABLES'")
        .await
        .expect("dirty session before change-user");
    connection
        .change_user(ChangeUserOpts::new())
        .await
        .expect("COM_CHANGE_USER");
    assert_eq!(connection.id(), connection_id);
    let reset_mode: Option<String> = connection
        .query_first("SELECT @@sql_mode")
        .await
        .expect("session after change-user");
    assert!(
        !reset_mode
            .as_deref()
            .unwrap_or_default()
            .contains("ANSI_QUOTES"),
        "change-user must restore default session state"
    );
    assert!(
        connection
            .exec_drop(&change_user_statement, (1_u64,))
            .await
            .is_err(),
        "change-user must invalidate prepared statements"
    );
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
    connection
        .query_drop("SET time_zone = '+05:30'")
        .await
        .expect("dirty pooled session");
    drop(connection);

    let mut connection = pool.get_conn().await.expect("reuse pooled connection");
    assert_eq!(
        connection.id(),
        connection_id,
        "pool should reuse the reset physical connection"
    );
    let pooled_zone: Option<String> = connection
        .query_first("SELECT @@session.time_zone")
        .await
        .expect("pooled session after automatic reset");
    assert_eq!(pooled_zone.as_deref(), Some("SYSTEM"));
    drop(connection);
    pool.disconnect().await.expect("disconnect wire client");

    let wrong = Pool::new(
        Opts::from_url(&format!("mysql://analytics:wrong@{address}/analytics"))
            .expect("wrong-key DSN"),
    );
    assert!(wrong.get_conn().await.is_err());
    wrong.disconnect().await.expect("disconnect rejected pool");

    let virtual_schema = Pool::new(
        Opts::from_url(&format!(
            "mysql://analytics:pk_wire_secret@{address}/information_schema"
        ))
        .expect("information_schema DSN"),
    );
    let mut virtual_connection = virtual_schema
        .get_conn()
        .await
        .expect("virtual information_schema connection");
    // Connecting with database=information_schema relaxes only the check
    // that the requested name matches the key's database. Authentication
    // still resolves the replica from the USERNAME and validates the key
    // against that database's keys, and the catalog handed to metadata is
    // built from that replica alone — so scoping holds by construction
    // rather than by this assertion. With a single-database fixture this
    // count cannot distinguish scoped from unscoped; it guards that the
    // virtual schema answers at all.
    let schema_rows: Option<u64> = virtual_connection
        .query_first("SELECT COUNT(*) FROM information_schema.schemata")
        .await
        .expect("query virtual information_schema");
    assert_eq!(schema_rows, Some(1));
    drop(virtual_connection);
    virtual_schema
        .disconnect()
        .await
        .expect("disconnect virtual information_schema pool");

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
        .unwrap_or_else(|error| panic!("run mysql CLI {}: {error}", mysql_cli.display()));
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
        .current_dir(&clients)
        .env("PINTAIL_WIRE_HOST", "127.0.0.1")
        .env("PINTAIL_WIRE_PORT", &port)
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

    let go = Command::new("go")
        .args(["run", "."])
        .current_dir(&clients)
        .env("PINTAIL_WIRE_HOST", "127.0.0.1")
        .env("PINTAIL_WIRE_PORT", &port)
        .output()
        .expect("run Go MySQL client");
    assert!(
        go.status.success(),
        "Go MySQL client failed: {}",
        String::from_utf8_lossy(&go.stderr)
    );
    let go_output = String::from_utf8_lossy(&go.stdout);
    assert!(go_output.contains(r#""bound_name":"land""#), "{go_output}");
    assert!(go_output.contains(r#""columns":2"#), "{go_output}");
    assert!(go_output.contains(r#""tables":2"#), "{go_output}");
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
        rows_are_exact: false,
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
                generation_expression: String::new(),
                extra: String::new(),
                auto_increment: true,
                default_value: None,
                default_generated: false,
                ordinal: 0,
            },
            SourceColumn {
                id: 2,
                name: "name".to_owned(),
                mysql_data_type: "varchar".to_owned(),
                mysql_column_type: "varchar(255)".to_owned(),
                pintail_type: DataType::Utf8,
                nullable: false,
                character_set: Some("utf8mb4".to_owned()),
                collation: Some("utf8mb4_0900_ai_ci".to_owned()),
                generated_stored: false,
                generation_expression: String::new(),
                extra: String::new(),
                auto_increment: false,
                default_value: None,
                default_generated: false,
                ordinal: 0,
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
        secondary_indexes: Vec::new(),
        warnings: Vec::new(),
        source_column_count: 0,
    }
}

#[allow(clippy::too_many_lines)]
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
        generation_expression: String::new(),
        extra: String::new(),
        auto_increment: false,
        default_value: None,
        default_generated: false,
        ordinal: 0,
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
                generation_expression: String::new(),
                extra: String::new(),
                auto_increment: false,
                default_value: None,
                default_generated: false,
                ordinal: 0,
            }
        },
    ));
    SourceTable {
        name: "type_fidelity".to_owned(),
        engine: Some("InnoDB".to_owned()),
        estimated_rows: Some(2),
        rows_are_exact: false,
        columns,
        key: SourceKey {
            mode: KeyMode::Primary,
            index_name: Some("PRIMARY".to_owned()),
            columns: vec!["id".to_owned()],
        },
        unique_keys: Vec::new(),
        requires_reconciliation: false,
        foreign_keys: Vec::new(),
        secondary_indexes: Vec::new(),
        warnings: Vec::new(),
        source_column_count: 0,
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

/// A saturated server must refuse a query with a real `MySQL` error the client
/// can act on, not hang and not drop the connection.
///
/// The load baseline showed the unbounded behaviour: p99 rose with
/// concurrency to 22s at 256 clients while nothing was ever refused. This
/// pins the opposite contract — past the bound the client is told, quickly.
#[tokio::test(flavor = "multi_thread")]
async fn a_saturated_server_refuses_queries_instead_of_queueing_them() {
    let _serial = wire_serial();
    // A one-slot bound makes saturation deterministic: the first query holds
    // the only permit while the second asks for one.
    pintail_wire::init_shared_admission(1);
    let admission = pintail_wire::shared_admission();
    // If another test in this binary installed the shared bound first, the
    // OnceLock kept that value and this test cannot observe refusal.
    if admission.limit() != 1 {
        return;
    }
    let held = admission.try_admit().expect("the only slot");

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
        pintail_wire::serve_until_with_options(
            listener,
            data_dir,
            server_metadata,
            pintail_wire::DEFAULT_QUERY_MEMORY_LIMIT,
            None,
            Duration::from_secs(30),
            async {
                let _ = shutdown_rx.await;
            },
        )
        .await
    });

    let pool = Pool::new(
        Opts::from_url(&format!(
            "mysql://analytics:pk_wire_secret@{address}/analytics"
        ))
        .expect("wire DSN"),
    );
    let mut connection = pool.get_conn().await.expect("authenticated wire client");
    // Connecting still works: the bound covers execution, not sessions.
    let refused = connection
        .query_drop("SELECT * FROM events")
        .await
        .expect_err("a saturated server must refuse the query");
    let message = refused.to_string();
    assert!(
        message.contains("concurrent queries"),
        "refusal must name the real cause, got: {message}"
    );

    // Releasing the slot must make the server usable again rather than
    // leaving it wedged.
    drop(held);
    connection
        .query_drop("SELECT * FROM events")
        .await
        .expect("the server must recover once a slot frees");

    drop(connection);
    pool.disconnect().await.ok();
    let _ = shutdown_tx.send(());
    server
        .await
        .expect("wire server task")
        .expect("wire server");
}

/// Seeds one LOCAL (writable) database and one replicated database in the
/// same control plane, each with its own wire key, so a single server can be
/// asked the same question about both.
fn seed_local_and_replicated(data_dir: &std::path::Path, metadata_path: &std::path::Path) {
    let metadata = MetaStore::open(metadata_path).expect("metadata");
    metadata
        .create_local_database("db-local", "scratch", "2026-09-05T00:00:00Z")
        .expect("local database");
    metadata
        .upsert_database("db-1", "analytics", b"unused", "2026-09-05T00:00:00Z")
        .expect("replicated database");
    // Distinct secrets: a key's sha256 is unique across the control plane.
    for (id, database, secret) in [
        ("key-local", "db-local", b"pk_wire_local_".as_slice()),
        ("key-replica", "db-1", b"pk_wire_replica".as_slice()),
    ] {
        let sha256 = Sha256::digest(secret);
        let native = Sha1::digest(Sha1::digest(secret));
        let caching_sha2 = Sha256::digest(Sha256::digest(secret));
        metadata
            .create_api_key(&NewApiKey {
                id,
                database_id: database,
                name: "wire gate",
                sha256: &sha256,
                mysql_native_password_hash: Some(&native),
                caching_sha2_password_hash: Some(&caching_sha2),
                scopes_json: r#"["query","read"]"#,
                expires_at: None,
                now: "2026-09-05T00:00:01Z",
            })
            .expect("wire key");
    }
    drop(metadata);
    std::fs::create_dir_all(data_dir.join("databases").join("db-local").join("tables"))
        .expect("local table root");
    pintail_write::LocalDatabase::new(data_dir, metadata_path, "db-local")
        .recover()
        .expect("publish the empty local catalog");
}

/// Every way a client can ask a local database for atomicity across
/// statements, and the write that outlives the `ROLLBACK` it sends after.
async fn assert_local_refuses_transactions(local: &mut mysql_async::Conn) {
    for sql in [
        "BEGIN",
        "START TRANSACTION",
        "COMMIT",
        "ROLLBACK",
        "SET autocommit = 0",
        "SET SESSION autocommit=0",
    ] {
        let error = local
            .query_drop(sql)
            .await
            .expect_err("a guarantee Pintail cannot keep must not answer OK");
        let message = error.to_string();
        assert!(
            message.contains("autocommit transaction"),
            "{sql} must say why it is refused, got: {message}"
        );
        // 1149: a valid statement that is unsupported here. A client
        // retrying on a syntax error would loop forever instead.
        assert!(
            matches!(&error, mysql_async::Error::Server(server) if server.code == 1149),
            "{sql} must carry MySQL's unsupported-statement code, got: {error}"
        );
    }

    // What the refusal protects: the insert is autocommitted, and no
    // ROLLBACK the client can send will take it back.
    local
        .query_drop("CREATE TABLE probe (id BIGINT UNSIGNED NOT NULL, PRIMARY KEY (id))")
        .await
        .expect("create table");
    local
        .query_drop("INSERT INTO probe (id) VALUES (1)")
        .await
        .expect("insert");
    let _ = local
        .query_drop("ROLLBACK")
        .await
        .expect_err("the rollback is still refused");
    let surviving: Option<i64> = local
        .query_first("SELECT COUNT(*) FROM probe")
        .await
        .expect("count");
    assert_eq!(surviving, Some(1), "the insert was committed by itself");

    // Autocommit stays settable to what it already is, and the isolation
    // level a driver announces describes an autocommitted statement fine.
    for sql in [
        "SET autocommit = 1",
        "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
    ] {
        local
            .query_drop(sql)
            .await
            .expect("accepted session command");
    }
}

/// The reproduction this closes: over the wire, `BEGIN` / `INSERT` /
/// `ROLLBACK` each returned OK on a local database and the row was still
/// there afterwards, so a client had every reason to believe its insert had
/// been undone.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_database_refuses_transactions_a_replica_still_accepts() {
    let _serial = wire_serial();
    let data = tempfile::tempdir().expect("wire data directory");
    let metadata_path = data.path().join("pintail-meta.db");
    seed_local_and_replicated(data.path(), &metadata_path);

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

    let connect = |user: &str, secret: &str| {
        let dsn = format!("mysql://{user}:{secret}@{address}/{user}");
        Pool::new(
            OptsBuilder::from_opts(Opts::from_url(&dsn).expect("wire DSN")).pool_opts(
                PoolOpts::default().with_constraints(
                    PoolConstraints::new(1, 1).expect("single-connection test pool"),
                ),
            ),
        )
    };

    let local_pool = connect("scratch", "pk_wire_local_");
    let mut local = local_pool.get_conn().await.expect("local wire client");
    assert_local_refuses_transactions(&mut local).await;

    // A replicated database writes nothing, so the compatibility no-op that
    // lets drivers and BI tools open a transaction before a SELECT stays.
    let replica_pool = connect("analytics", "pk_wire_replica");
    let mut replica = replica_pool
        .get_conn()
        .await
        .expect("replicated wire client");
    for sql in [
        "BEGIN",
        "START TRANSACTION",
        "COMMIT",
        "ROLLBACK",
        "SET autocommit = 0",
    ] {
        replica
            .query_drop(sql)
            .await
            .expect("read-only sessions keep the compatibility no-op");
    }

    drop(local);
    drop(replica);
    local_pool.disconnect().await.expect("close local pool");
    replica_pool.disconnect().await.expect("close replica pool");
    let _ = shutdown_tx.send(());
    server
        .await
        .expect("wire server task")
        .expect("wire server");
}

/// A listener with explicit bounds, over the local-plus-replicated seed.
async fn serve_with_limits(
    data: &tempfile::TempDir,
    limits: pintail_wire::WireLimits,
) -> (
    std::net::SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let metadata_path = data.path().join("pintail-meta.db");
    seed_local_and_replicated(data.path(), &metadata_path);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("wire listener");
    let address = listener.local_addr().expect("wire address");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let data_dir = data.path().to_path_buf();
    let server = tokio::spawn(async move {
        pintail_wire::serve_until_configured(
            listener,
            data_dir,
            metadata_path,
            pintail_wire::WireOptions {
                query_memory_limit: pintail_wire::DEFAULT_QUERY_MEMORY_LIMIT,
                tls: None,
                idle_timeout: Duration::from_secs(30),
                limits,
            },
            async {
                let _ = shutdown_rx.await;
            },
        )
        .await
    });
    (address, shutdown_tx, server)
}

/// The connection ceiling counts from accept, not from authentication: a
/// client that connects and never logs in holds a slot too, which is
/// exactly the client an unbounded listener could not defend against.
#[tokio::test(flavor = "multi_thread")]
async fn the_connection_ceiling_refuses_with_1040_and_frees_on_close() {
    use tokio::io::AsyncReadExt as _;
    let _serial = wire_serial();
    let data = tempfile::tempdir().expect("wire data directory");
    let (address, shutdown_tx, server) = serve_with_limits(
        &data,
        pintail_wire::WireLimits {
            max_connections: 2,
            ..pintail_wire::WireLimits::default()
        },
    )
    .await;
    let before = pintail_wire::wire_metrics();

    // Two raw sockets that read the greeting and then sit there, unauthenticated.
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut socket = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect under the ceiling");
        let mut greeting = [0u8; 4];
        socket
            .read_exact(&mut greeting)
            .await
            .expect("a greeting proves the slot was granted");
        held.push(socket);
    }

    let dsn = format!("mysql://analytics:pk_wire_replica@{address}/analytics");
    let refused = mysql_async::Conn::new(Opts::from_url(&dsn).expect("wire DSN"))
        .await
        .expect_err("the third connection is over the ceiling");
    assert!(
        matches!(&refused, mysql_async::Error::Server(server) if server.code == 1040),
        "a refusal must be MySQL's own 'Too many connections', got: {refused}"
    );
    let during = pintail_wire::wire_metrics();
    assert_eq!(during.connections_refused, before.connections_refused + 1);
    assert_eq!(during.connections_active, before.connections_active + 2);
    assert_eq!(during.connections_limit, 2);

    // Closing one held socket frees its slot once the server notices.
    drop(held.pop());
    let mut admitted = None;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(connection) =
            mysql_async::Conn::new(Opts::from_url(&dsn).expect("wire DSN")).await
        {
            admitted = Some(connection);
            break;
        }
    }
    let mut admitted = admitted.expect("a freed slot admits the next client");
    admitted
        .ping()
        .await
        .expect("an admitted client is fully served");
    admitted.disconnect().await.ok();
    drop(held);

    let _ = shutdown_tx.send(());
    server
        .await
        .expect("wire server task")
        .expect("wire server");
}

/// Both prepared-statement bounds, and that closing a statement gives its
/// allowance back - the case a driver's statement cache relies on.
#[tokio::test(flavor = "multi_thread")]
async fn the_prepared_statement_ceilings_refuse_with_1461_and_free_on_close() {
    use mysql_async::prelude::Queryable as _;
    let _serial = wire_serial();
    let data = tempfile::tempdir().expect("wire data directory");
    let (address, shutdown_tx, server) = serve_with_limits(
        &data,
        pintail_wire::WireLimits {
            max_prepared_statements: 2,
            max_prepared_statement_bytes: 64,
            ..pintail_wire::WireLimits::default()
        },
    )
    .await;
    let before = pintail_wire::wire_metrics().prepared_statements_refused;
    // The local database: PREPARE previews the statement against a loaded
    // replica, and the seeded replicated one has never been probed.
    let dsn = format!("mysql://scratch:pk_wire_local_@{address}/scratch");
    let mut connection = mysql_async::Conn::new(Opts::from_url(&dsn).expect("wire DSN"))
        .await
        .expect("wire client");
    connection
        .query_drop("CREATE TABLE probe (id BIGINT UNSIGNED NOT NULL, PRIMARY KEY (id))")
        .await
        .expect("a table, so the catalog has something to load");

    // Distinct texts: the client caches by SQL and would not send a
    // second PREPARE for a repeat.
    let first = connection.prep("SELECT 1").await.expect("first statement");
    let _second = connection.prep("SELECT 2").await.expect("second statement");
    let refused = connection
        .prep("SELECT 3")
        .await
        .expect_err("the third statement is over the count ceiling");
    assert!(
        matches!(&refused, mysql_async::Error::Server(server) if server.code == 1461),
        "a refusal must be MySQL's own max_prepared_stmt_count error, got: {refused}"
    );

    // Closing one gives the slot back.
    connection.close(first).await.expect("close");
    let _third = connection.prep("SELECT 3").await.expect("a freed slot");

    // The byte ceiling applies even under the count: 64 bytes of text
    // across the session, and this one statement is longer than that.
    let wide = format!("SELECT {}", "1 + ".repeat(40).trim_end_matches(" + "));
    assert!(wide.len() > 64);
    let refused = connection
        .prep(wide.as_str())
        .await
        .expect_err("statement text over the byte ceiling");
    assert!(
        matches!(&refused, mysql_async::Error::Server(server) if server.code == 1461),
        "got: {refused}"
    );
    assert_eq!(
        pintail_wire::wire_metrics().prepared_statements_refused,
        before + 2,
        "both refusals were counted"
    );

    connection.disconnect().await.ok();
    let _ = shutdown_tx.send(());
    server
        .await
        .expect("wire server task")
        .expect("wire server");
}
