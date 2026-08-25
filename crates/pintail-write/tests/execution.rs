//! The write path end to end: publish a table, commit rows, and prove they
//! survive a reopen. These run entirely in process — no docker, no server —
//! so the ordering rules in `docs/design/writable-mode.md` are testable at
//! unit speed.

use pintail_meta::MetaStore;
use pintail_sql::parse_statement;
use pintail_store::{TableSnapshot, table_directory};
use pintail_types::Value;
use pintail_write::{LocalDatabase, WriteOutcome};

struct Fixture {
    _directory: tempfile::TempDir,
    data_dir: std::path::PathBuf,
    metadata_path: std::path::PathBuf,
    database: LocalDatabase,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let data_dir = directory.path().to_path_buf();
    let metadata_path = data_dir.join("pintail-meta.db");
    let metadata = MetaStore::open(&metadata_path).expect("metadata");
    metadata
        .create_local_database("db-local", "scratch", "2026-08-24T00:00:00Z")
        .expect("create local database");
    drop(metadata);
    let database = LocalDatabase::new(&data_dir, &metadata_path, "db-local");
    Fixture {
        _directory: directory,
        data_dir,
        metadata_path,
        database,
    }
}

fn run(fixture: &Fixture, sql: &str) -> Result<WriteOutcome, pintail_write::WriteError> {
    fixture
        .database
        .execute(&parse_statement(sql).expect("parses"))
}

/// Reads the table back through the ordinary snapshot reader, which is what
/// a query would use — not through the writer's own state.
fn stored_rows(fixture: &Fixture, table: &str) -> Vec<Vec<Value>> {
    let catalog = fixture.database.catalog().expect("catalog");
    let source = catalog
        .iter()
        .find(|candidate| candidate.name == table)
        .unwrap_or_else(|| panic!("{table} is published"));
    let schema = source.table_schema_with_version(1).expect("schema");
    let root = fixture
        .data_dir
        .join("databases")
        .join("db-local")
        .join("tables");
    let snapshot =
        TableSnapshot::open(table_directory(&root, table), schema).expect("open snapshot");
    let mut rows = snapshot
        .scan()
        .expect("scan")
        .into_iter()
        .map(|row| row.values().to_vec())
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
    rows
}

#[test]
fn a_table_is_created_and_rows_committed_survive_a_reopen() {
    let fixture = fixture();

    let created = run(
        &fixture,
        "CREATE TABLE notes (id BIGINT UNSIGNED NOT NULL, body VARCHAR(64) NOT NULL, \
         PRIMARY KEY (id))",
    )
    .expect("create");
    assert_eq!(
        created,
        WriteOutcome::TableCreated {
            table: "notes".to_owned(),
            existed: false
        }
    );

    let inserted = run(
        &fixture,
        "INSERT INTO notes (id, body) VALUES (1, 'first'), (2, 'second')",
    )
    .expect("insert");
    let WriteOutcome::RowsInserted { rows, version } = inserted else {
        panic!("expected an insert outcome");
    };
    assert_eq!(rows, 2);
    assert_eq!(version, 1, "the first commit is version 1");

    // A second statement is its own transaction and its own version.
    let WriteOutcome::RowsInserted {
        version: second, ..
    } = run(&fixture, "INSERT INTO notes (id, body) VALUES (3, 'third')").expect("insert")
    else {
        panic!("expected an insert outcome");
    };
    assert_eq!(second, 2);

    // Re-open everything from disk, exactly as a restarted process would.
    let reopened = LocalDatabase::new(&fixture.data_dir, &fixture.metadata_path, "db-local");
    let catalog = reopened.catalog().expect("catalog survives");
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].name, "notes");
    assert_eq!(catalog[0].key.columns, ["id"]);

    assert_eq!(
        stored_rows(&fixture, "notes"),
        [
            vec![Value::UInt64(1), Value::Utf8("first".to_owned())],
            vec![Value::UInt64(2), Value::Utf8("second".to_owned())],
            vec![Value::UInt64(3), Value::Utf8("third".to_owned())],
        ]
    );
}

#[test]
fn a_key_already_stored_is_refused_as_1062() {
    let fixture = fixture();
    run(
        &fixture,
        "CREATE TABLE notes (id BIGINT UNSIGNED, body TEXT, PRIMARY KEY (id))",
    )
    .expect("create");
    run(&fixture, "INSERT INTO notes (id, body) VALUES (1, 'first')").expect("insert");

    // The duplicate is against a COMMITTED row, not another row in the same
    // statement - the binder catches that case, this one needs the store.
    let error = run(&fixture, "INSERT INTO notes (id, body) VALUES (1, 'again')")
        .expect_err("duplicate refused");
    assert_eq!(error.mysql_code(), 1062);

    // The refused statement committed nothing.
    assert_eq!(
        stored_rows(&fixture, "notes"),
        [vec![Value::UInt64(1), Value::Utf8("first".to_owned())]]
    );
}

#[test]
fn a_partly_duplicate_batch_commits_nothing() {
    let fixture = fixture();
    run(
        &fixture,
        "CREATE TABLE notes (id BIGINT UNSIGNED, body TEXT, PRIMARY KEY (id))",
    )
    .expect("create");
    run(&fixture, "INSERT INTO notes (id, body) VALUES (1, 'first')").expect("insert");

    // Row 2 is new and row 1 collides: the whole statement must fail, or an
    // autocommit INSERT would be partially applied.
    let error = run(
        &fixture,
        "INSERT INTO notes (id, body) VALUES (2, 'new'), (1, 'collides')",
    )
    .expect_err("refused");
    assert_eq!(error.mysql_code(), 1062);
    assert_eq!(
        stored_rows(&fixture, "notes").len(),
        1,
        "the new row must not have landed either"
    );
}

#[test]
fn creating_the_same_table_twice_is_1050_unless_if_not_exists() {
    let fixture = fixture();
    let create = "CREATE TABLE notes (id BIGINT PRIMARY KEY, body TEXT)";
    run(&fixture, create).expect("create");

    let error = run(&fixture, create).expect_err("second create");
    assert_eq!(error.mysql_code(), 1050);

    let repeated = run(
        &fixture,
        "CREATE TABLE IF NOT EXISTS notes (id BIGINT PRIMARY KEY, body TEXT)",
    )
    .expect("if not exists");
    assert_eq!(
        repeated,
        WriteOutcome::TableCreated {
            table: "notes".to_owned(),
            existed: true
        }
    );
    assert_eq!(fixture.database.catalog().unwrap().len(), 1);
}

#[test]
fn a_table_left_mid_creation_is_removed_by_recovery() {
    let fixture = fixture();
    run(
        &fixture,
        "CREATE TABLE kept (id BIGINT PRIMARY KEY, body TEXT)",
    )
    .expect("create");

    // Simulate a crash between the catalog row and its publication: the row
    // exists as 'creating' and nothing else does.
    let metadata = MetaStore::open(&fixture.metadata_path).expect("metadata");
    metadata
        .begin_local_table("db-local", "half_built", r#"["id"]"#)
        .expect("register");
    drop(metadata);

    let removed = fixture.database.recover().expect("recover");
    assert_eq!(removed, ["half_built"]);

    let catalog = fixture.database.catalog().expect("catalog");
    assert_eq!(catalog.len(), 1, "only the published table survives");
    assert_eq!(catalog[0].name, "kept");
    // Recovery is idempotent.
    assert!(
        fixture
            .database
            .recover()
            .expect("recover again")
            .is_empty()
    );
}

#[test]
fn a_local_database_starts_with_no_tables() {
    let fixture = fixture();
    assert!(fixture.database.catalog().expect("catalog").is_empty());
    // Inserting into a table that was never created is 1146, not a panic.
    let error = run(&fixture, "INSERT INTO nothing (id) VALUES (1)").expect_err("no table");
    assert_eq!(error.mysql_code(), 1146);
}

#[test]
fn statements_a_local_database_does_not_accept_are_refused() {
    let fixture = fixture();
    for sql in [
        "UPDATE notes SET body = 'x' WHERE id = 1",
        "DELETE FROM notes WHERE id = 1",
        "DROP TABLE notes",
    ] {
        // Phases 3 and 4 add these; until then they must refuse rather than
        // silently do nothing.
        assert!(run(&fixture, sql).is_err(), "must refuse: {sql}");
    }
}
