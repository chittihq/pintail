//! Writes reaching a LOCAL database through the same engine entry point a
//! `MySQL` client uses, and the read that must see them afterwards.
//!
//! The point of exercising `ReplicaEngine` rather than the write crate
//! directly: this is where a write and a read meet. A `CREATE TABLE` that
//! does not invalidate the cached replica, or an `INSERT` whose rows the
//! next `SELECT` cannot see, only fails here.

use pintail_meta::MetaStore;
use pintail_wire::ReplicaEngine;
use pintail_write::LocalDatabase;

struct Fixture {
    _directory: tempfile::TempDir,
    engine: ReplicaEngine,
    metadata_path: std::path::PathBuf,
}

fn local_fixture() -> Fixture {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let data_dir = directory.path().to_path_buf();
    let metadata_path = data_dir.join("pintail-meta.db");
    let metadata = MetaStore::open(&metadata_path).expect("metadata");
    metadata
        .create_local_database("db-local", "scratch", "2026-08-24T00:00:00Z")
        .expect("create local database");
    drop(metadata);
    std::fs::create_dir_all(data_dir.join("databases").join("db-local").join("tables"))
        .expect("table root");
    // Publishes the empty catalog, exactly as the creation endpoint does.
    LocalDatabase::new(&data_dir, &metadata_path, "db-local")
        .recover()
        .expect("initialize catalog");

    Fixture {
        _directory: directory,
        engine: ReplicaEngine::new(&data_dir, &metadata_path),
        metadata_path,
    }
}

fn run(
    fixture: &Fixture,
    sql: &str,
) -> Result<pintail_wire::QueryOutput, pintail_wire::QueryError> {
    fixture.engine.execute("db-local", sql, 1_000)
}

#[test]
fn a_local_database_creates_inserts_and_reads_back() {
    let fixture = local_fixture();

    let created = run(
        &fixture,
        "CREATE TABLE notes (id BIGINT UNSIGNED NOT NULL, body VARCHAR(64) NOT NULL, \
         PRIMARY KEY (id))",
    )
    .expect("create table");
    // DDL answers as a write with no rows changed, so the server sends an OK
    // packet rather than an empty result set.
    assert_eq!(created.affected, Some(0));

    let inserted = run(
        &fixture,
        "INSERT INTO notes (id, body) VALUES (1, 'alpha'), (2, 'beta')",
    )
    .expect("insert");
    assert_eq!(
        inserted.affected,
        Some(2),
        "the client reads this as affected_rows"
    );

    // The read must see the write: this is the case a stale cached replica
    // would fail, and it goes through the ordinary SELECT path.
    let selected = run(&fixture, "SELECT id, body FROM notes ORDER BY id").expect("select");
    assert_eq!(selected.affected, None, "a read is never a write");
    assert_eq!(selected.rows.len(), 2);
    assert_eq!(
        selected.rows[0][1],
        pintail_types::Value::Utf8("alpha".to_owned())
    );
    assert_eq!(
        selected.rows[1][1],
        pintail_types::Value::Utf8("beta".to_owned())
    );

    // A second table proves the catalog keeps growing rather than being
    // replaced by the newest publication.
    run(
        &fixture,
        "CREATE TABLE tags (id BIGINT PRIMARY KEY, label TEXT)",
    )
    .expect("second table");
    run(&fixture, "INSERT INTO tags (id, label) VALUES (9, 'x')").expect("insert into second");
    assert_eq!(run(&fixture, "SELECT id FROM tags").unwrap().rows.len(), 1);
    assert_eq!(
        run(&fixture, "SELECT COUNT(*) FROM notes").unwrap().rows[0][0],
        pintail_types::Value::UInt64(2),
        "the first table is still readable after the second was created"
    );
}

#[test]
fn write_rejections_carry_their_mysql_codes() {
    let fixture = local_fixture();
    run(
        &fixture,
        "CREATE TABLE notes (id BIGINT UNSIGNED, body TEXT NOT NULL, PRIMARY KEY (id))",
    )
    .expect("create");
    run(&fixture, "INSERT INTO notes (id, body) VALUES (1, 'alpha')").expect("insert");

    let cases = [
        (
            "CREATE TABLE notes (id BIGINT PRIMARY KEY)",
            pintail_wire::SqlRejection::TableExists,
        ),
        (
            "INSERT INTO notes (id, body) VALUES (1, 'again')",
            pintail_wire::SqlRejection::DuplicateKey,
        ),
        (
            "INSERT INTO notes (id, body) VALUES (2, NULL)",
            pintail_wire::SqlRejection::NotNull,
        ),
        (
            "INSERT INTO absent (id) VALUES (1)",
            pintail_wire::SqlRejection::UnknownTable,
        ),
        (
            "INSERT INTO notes (id, nope) VALUES (2, 'x')",
            pintail_wire::SqlRejection::UnknownColumn,
        ),
    ];
    for (sql, expected) in cases {
        match run(&fixture, sql) {
            Err(pintail_wire::QueryError::Rejected { rejection, .. }) => {
                assert_eq!(rejection, expected, "for {sql}");
            }
            other => panic!("{sql} must be rejected with a code, got {other:?}"),
        }
    }
}

#[test]
fn a_replicated_database_still_refuses_every_write() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let data_dir = directory.path().to_path_buf();
    let metadata_path = data_dir.join("pintail-meta.db");
    let metadata = MetaStore::open(&metadata_path).expect("metadata");
    metadata
        .upsert_database("db-1", "shop", b"secret", "2026-08-24T00:00:00Z")
        .expect("replicated database");
    drop(metadata);
    let engine = ReplicaEngine::new(&data_dir, &metadata_path);

    for sql in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY)",
        "INSERT INTO t (id) VALUES (1)",
    ] {
        let error = engine.execute("db-1", sql, 10).expect_err("refused");
        assert!(
            error.to_string().contains("read-only"),
            "{sql} must stay read-only on a replica, got {error}"
        );
    }
}

#[test]
fn a_write_against_a_missing_database_is_not_treated_as_writable() {
    let fixture = local_fixture();
    // A database that does not exist must not fall through to the write
    // path: the kind lookup answers false, which is the read-only refusal.
    let error = fixture
        .engine
        .execute("db-missing", "INSERT INTO t (id) VALUES (1)", 10)
        .expect_err("refused");
    assert!(error.to_string().contains("read-only"), "{error}");
    let _ = &fixture.metadata_path;
}

#[test]
fn a_keyless_local_table_keeps_every_row() {
    let fixture = local_fixture();
    run(
        &fixture,
        "CREATE TABLE log (line VARCHAR(16) NOT NULL, n INT)",
    )
    .expect("create keyless table");
    run(
        &fixture,
        "INSERT INTO log (line, n) VALUES ('a', 1), ('b', 2)",
    )
    .expect("first insert");
    // The same row again is a second row, as it is in MySQL: nothing
    // about a keyless table can call it a duplicate.
    run(&fixture, "INSERT INTO log (line, n) VALUES ('a', 1)").expect("second insert");

    assert_eq!(
        run(&fixture, "SELECT COUNT(*) FROM log").unwrap().rows[0][0],
        pintail_types::Value::UInt64(3)
    );
    let lines = run(&fixture, "SELECT line FROM log ORDER BY line, n").expect("select");
    assert_eq!(
        lines
            .rows
            .iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>(),
        ["a", "a", "b"]
            .into_iter()
            .map(|text| pintail_types::Value::Utf8(text.to_owned()))
            .collect::<Vec<_>>()
    );
    // The generated row id is storage's business, never a column.
    let all = run(&fixture, "SELECT * FROM log").expect("select star");
    assert_eq!(all.fields.len(), 2, "{:?}", all.fields);
}

#[test]
fn bit_operators_evaluate_over_unsigned_64_bit_patterns() {
    let fixture = local_fixture();
    let output = run(
        &fixture,
        "SELECT 6 & 3, 6 | 3, 6 ^ 3, 1 << 62, -1 | 0, 1 >> 64, NULL & 1",
    )
    .expect("select");
    let expected = [
        pintail_types::Value::UInt64(2),
        pintail_types::Value::UInt64(7),
        pintail_types::Value::UInt64(5),
        pintail_types::Value::UInt64(1 << 62),
        // -1 as a 64-bit pattern, the way MySQL reads it.
        pintail_types::Value::UInt64(u64::MAX),
        pintail_types::Value::UInt64(0),
        pintail_types::Value::Null,
    ];
    assert_eq!(output.rows[0], expected);
}

#[test]
fn insert_and_time_follow_mysql() {
    let fixture = local_fixture();
    let output = run(
        &fixture,
        "SELECT INSERT('Quadratic', 3, 4, 'What'), INSERT('Quadratic', -1, 4, 'What'), \
         INSERT('Quadratic', 3, 100, 'What'), INSERT('Quadratic', 10, 1, 'X'), \
         TIME('2003-12-31 01:02:03'), TIME('01:02:03'), TIME('2003-12-31 01:02:03.000123')",
    )
    .expect("select");
    let text = |value: &str| pintail_types::Value::Utf8(value.to_owned());
    assert_eq!(
        output.rows[0],
        [
            text("QuWhattic"),
            text("Quadratic"),
            text("QuWhat"),
            text("QuadraticX"),
            text("01:02:03"),
            text("01:02:03"),
            text("01:02:03.000123"),
        ]
    );
}
