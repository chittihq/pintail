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

#[test]
fn temporal_writes_round_once_at_declared_precision_or_truncate() {
    for (mode, duration, moment) in [
        ("", "-00:00:00.111112", "1960-01-01 00:00:01.000000"),
        (
            "TIME_TRUNCATE_FRACTIONAL",
            "-00:00:00.111111",
            "1960-01-01 00:00:00.999999",
        ),
    ] {
        let fixture = fixture();
        pintail_sql::with_parse_mode(pintail_sql::ParseMode::from_sql_mode(mode), || {
            run(&fixture, "CREATE TABLE clocks (id BIGINT PRIMARY KEY, duration TIME(6), short_duration TIME(3), moment DATETIME(6))").unwrap();
            run(&fixture, "INSERT INTO clocks VALUES (1, '-00:00:00.1111115', '00:00:00.123499999', '1960-01-01 00:00:00.9999995')").unwrap();
        });
        assert_eq!(
            stored_rows(&fixture, "clocks"),
            vec![vec![
                Value::Int64(1),
                Value::Utf8(duration.into()),
                Value::Utf8("00:00:00.123".into()),
                Value::Utf8(moment.into())
            ]]
        );
    }
}

#[test]
fn enum_and_set_store_labels_with_the_declared_case() {
    let fixture = fixture();
    run(
        &fixture,
        "CREATE TABLE labels (choice ENUM('Alpha','Beta'), choices SET('Alpha','Beta'))",
    )
    .unwrap();
    run(&fixture, "INSERT INTO labels VALUES ('alpha','beta,alpha')").unwrap();
    assert_eq!(
        stored_rows(&fixture, "labels"),
        vec![vec![
            Value::Utf8("Alpha".into()),
            Value::Utf8("Alpha,Beta".into())
        ]]
    );
}

#[test]
fn float_bit_precision_selects_single_or_double_storage() {
    let fixture = fixture();
    run(
        &fixture,
        "CREATE TABLE readings (narrow FLOAT(24), wide FLOAT(52))",
    )
    .unwrap();
    run(&fixture, "INSERT INTO readings VALUES (1e-150, 1e-150)").unwrap();
    assert_eq!(
        stored_rows(&fixture, "readings"),
        vec![vec![Value::float64(0.0), Value::float64(1e-150)]]
    );
    let catalog = fixture.database.catalog().unwrap();
    assert_eq!(
        catalog[0].columns[1].pintail_type,
        pintail_types::DataType::Float64
    );
    assert_eq!(catalog[0].columns[1].mysql_data_type, "double");
}

#[test]
fn scientific_number_literals_choose_text_that_fits_the_column() {
    let fixture = fixture();
    run(&fixture, "CREATE TABLE readings (label CHAR(6))").unwrap();
    run(
        &fixture,
        "INSERT INTO readings VALUES (2e5),(2e6),(2e-4),(2e-5)",
    )
    .unwrap();
    assert_eq!(
        stored_rows(&fixture, "readings"),
        ["0.0002", "200000", "2e-5", "2e6"]
            .map(|text| vec![Value::Utf8(text.into())])
            .to_vec()
    );
}

#[test]
fn permissive_float_writes_clamp_to_declared_ranges() {
    let fixture = fixture();
    pintail_sql::with_parse_mode(pintail_sql::ParseMode::from_sql_mode(""), || {
        run(
            &fixture,
            "CREATE TABLE readings (narrow FLOAT, fixed DOUBLE(4,3), positive DOUBLE UNSIGNED)",
        )
        .unwrap();
        run(&fixture, "INSERT INTO readings VALUES (1e150, -11, -2)").unwrap();
    });
    assert_eq!(
        stored_rows(&fixture, "readings"),
        vec![vec![
            Value::float64(f64::from(f32::MAX)),
            Value::float64(-9.999),
            Value::float64(0.0)
        ]]
    );
    pintail_sql::with_parse_mode(
        pintail_sql::ParseMode::from_sql_mode("STRICT_ALL_TABLES"),
        || {
            assert!(run(&fixture, "INSERT INTO readings VALUES (1e150, 0, 0)").is_err());
            assert!(run(&fixture, "INSERT INTO readings VALUES (0, 11, 0)").is_err());
            assert!(run(&fixture, "INSERT INTO readings VALUES (0, 0, -1)").is_err());
        },
    );
}

#[test]
fn wide_fixed_float_bounds_use_correctly_rounded_decimal_powers() {
    let fixture = fixture();
    pintail_sql::with_parse_mode(pintail_sql::ParseMode::from_sql_mode(""), || {
        run(&fixture, "CREATE TABLE readings (reading DOUBLE(200,0))").unwrap();
        run(&fixture, "INSERT INTO readings VALUES (2e200),(-2e200)").unwrap();
    });
    let rows = stored_rows(&fixture, "readings");
    assert!(rows.contains(&vec![Value::float64(1e200)]));
    assert!(rows.contains(&vec![Value::float64(-1e200)]));
}

#[test]
fn binary_enum_and_set_members_keep_case_distinct() {
    let fixture = fixture();
    run(&fixture, "CREATE TABLE labels (choice ENUM('a','A') COLLATE utf8mb4_bin, choices SET('a','A') COLLATE utf8mb4_bin)").unwrap();
    run(&fixture, "INSERT INTO labels VALUES ('A','A,a'),('a','A')").unwrap();
    assert_eq!(
        stored_rows(&fixture, "labels"),
        vec![
            vec![Value::Utf8("A".into()), Value::Utf8("a,A".into())],
            vec![Value::Utf8("a".into()), Value::Utf8("A".into())]
        ]
    );
}

#[test]
fn stored_text_obeys_column_repertoires_and_strict_inserts_are_atomic() {
    let fixture = fixture();
    run(&fixture, "CREATE TABLE labels (id INT PRIMARY KEY, western VARCHAR(20) CHARACTER SET latin1, plain TEXT CHARACTER SET ascii, basic CHAR(8) CHARACTER SET utf8mb3, full TEXT CHARACTER SET utf8mb4)").unwrap();
    pintail_sql::with_parse_mode(pintail_sql::ParseMode::from_sql_mode(""), || {
        run(
            &fixture,
            "INSERT INTO labels VALUES (1, 'café €�😀', 'café😀', 'x😀  ', 'x😀')",
        )
        .unwrap();
    });
    let expected = vec![vec![
        Value::Int64(1),
        Value::Utf8("café €??".into()),
        Value::Utf8("caf??".into()),
        Value::Utf8("x?".into()),
        Value::Utf8("x😀".into()),
    ]];
    assert_eq!(stored_rows(&fixture, "labels"), expected);
    pintail_sql::with_parse_mode(
        pintail_sql::ParseMode::from_sql_mode("STRICT_TRANS_TABLES"),
        || {
            for values in [
                "'bad�', 'ok', 'ok'",
                "'ok', 'badé', 'ok'",
                "'ok', 'ok', 'bad😀'",
            ] {
                let error = run(&fixture, &format!("INSERT INTO labels VALUES (2, 'café €', 'ok', 'ok', '😀'), (3, {values}, '😀')")).unwrap_err();
                assert_eq!(error.mysql_code(), 1366);
                assert_eq!(error.sqlstate(), "HY000");
                assert!(error.to_string().ends_with("at row 2"));
                assert_eq!(stored_rows(&fixture, "labels"), expected);
            }
        },
    );
}

#[test]
fn binary_character_set_declarations_store_bytes_and_padding() {
    let fixture = fixture();
    run(&fixture, "CREATE TABLE labels (fixed_payload CHAR(4) CHARACTER SET binary, variable_payload VARCHAR(4) CHARACTER SET binary, large_payload TEXT CHARACTER SET binary)").unwrap();
    run(&fixture, "INSERT INTO labels VALUES ('é ', 'é ', 'é ')").unwrap();
    assert_eq!(
        stored_rows(&fixture, "labels"),
        vec![vec![
            Value::Binary(vec![0xc3, 0xa9, b' ', 0]),
            Value::Binary(vec![0xc3, 0xa9, b' ']),
            Value::Binary(vec![0xc3, 0xa9, b' ']),
        ]]
    );
    let catalog = fixture.database.catalog().unwrap();
    for (column, expected_type) in catalog[0]
        .columns
        .iter()
        .zip(["binary", "varbinary", "blob"])
    {
        assert_eq!(column.mysql_data_type, expected_type);
        assert_eq!(column.pintail_type, pintail_types::DataType::Binary);
        assert_eq!(column.character_set, None);
        assert_eq!(column.collation, None);
    }
}
