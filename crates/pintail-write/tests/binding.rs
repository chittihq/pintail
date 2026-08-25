//! What a local database accepts, and what it refuses with which `MySQL`
//! error. Every rejection here is a code a client branches on, so the
//! numbers are part of the contract rather than an implementation detail.

use pintail_sql::parse_statement;
use pintail_types::{DataType, KeyPart, Value};
use pintail_write::{WriteError, bind_create_table, bind_insert};

fn create(sql: &str) -> Result<pintail_write::CreateTablePlan, WriteError> {
    bind_create_table(&parse_statement(sql).expect("parses"))
}

/// A table the INSERT tests share: two key shapes and a nullable column.
fn notes_table() -> pintail_probe::SourceTable {
    create(
        "CREATE TABLE notes (\
            id BIGINT UNSIGNED NOT NULL, \
            body VARCHAR(64) NOT NULL, \
            note TEXT, \
            PRIMARY KEY (id))",
    )
    .expect("notes binds")
    .table
}

fn insert(sql: &str) -> Result<pintail_write::InsertPlan, WriteError> {
    bind_insert(&parse_statement(sql).expect("parses"), &notes_table())
}

#[test]
fn a_created_table_carries_probe_identical_types() {
    let plan = create(
        "CREATE TABLE t (\
            id BIGINT UNSIGNED NOT NULL, \
            small TINYINT, \
            flag TINYINT(1), \
            label VARCHAR(32) NOT NULL, \
            amount DECIMAL(12,2), \
            payload JSON, \
            PRIMARY KEY (id))",
    )
    .expect("binds");

    let types: Vec<DataType> = plan
        .table
        .columns
        .iter()
        .map(|column| column.pintail_type)
        .collect();
    assert_eq!(
        types,
        [
            DataType::UInt64,
            DataType::Int8,
            DataType::Boolean,
            DataType::Utf8,
            DataType::Decimal {
                precision: 12,
                scale: 2
            },
            DataType::Json,
        ]
    );
    assert_eq!(plan.table.key.columns, ["id"]);
    // Stable one-based column IDs, exactly as a probe assigns ordinals.
    assert_eq!(
        plan.table
            .columns
            .iter()
            .map(|column| column.id)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn an_inline_primary_key_is_equivalent_to_the_table_constraint() {
    let inline = create("CREATE TABLE t (id BIGINT PRIMARY KEY, body TEXT)").expect("inline");
    let constraint =
        create("CREATE TABLE t (id BIGINT, body TEXT, PRIMARY KEY (id))").expect("constraint");
    assert_eq!(inline.table.key.columns, constraint.table.key.columns);
    // MySQL makes a primary-key column NOT NULL whichever way it was
    // declared, even though neither statement said NOT NULL.
    assert!(!inline.table.columns[0].nullable);
    assert!(!constraint.table.columns[0].nullable);
}

#[test]
fn a_compound_primary_key_keeps_its_declared_order() {
    let plan = create("CREATE TABLE t (a BIGINT, b VARCHAR(8), c INT, PRIMARY KEY (b, a))")
        .expect("binds");
    assert_eq!(plan.table.key.columns, ["b", "a"]);
}

#[test]
fn a_table_without_a_primary_key_is_refused() {
    // A local table with no key has no row identity: nothing could detect a
    // duplicate, and UPDATE/DELETE could never address a row.
    let error = create("CREATE TABLE t (id BIGINT, body TEXT)").expect_err("refused");
    assert_eq!(error.mysql_code(), 1064);
    assert!(error.to_string().contains("PRIMARY KEY"), "{error}");
}

#[test]
fn out_of_scope_table_features_are_refused_by_name() {
    for sql in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, b INT, UNIQUE (b))",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, b INT, CHECK (b > 0))",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, b INT, FOREIGN KEY (b) REFERENCES o(id))",
        "CREATE TEMPORARY TABLE t (id BIGINT PRIMARY KEY)",
        "CREATE TABLE t AS SELECT 1",
    ] {
        assert!(create(sql).is_err(), "must refuse: {sql}");
    }
}

#[test]
fn a_duplicate_column_is_refused() {
    let error = create("CREATE TABLE t (id BIGINT PRIMARY KEY, body TEXT, body TEXT)")
        .expect_err("refused");
    assert!(error.to_string().contains("Duplicate column"), "{error}");
}

#[test]
fn a_key_naming_a_missing_column_is_refused() {
    let error = create("CREATE TABLE t (id BIGINT, PRIMARY KEY (nope))").expect_err("refused");
    assert!(error.to_string().contains("nope"), "{error}");
}

#[test]
fn insert_types_and_keys_every_row() {
    let plan =
        insert("INSERT INTO notes (id, body, note) VALUES (1, 'first', 'x'), (2, 'second', NULL)")
            .expect("binds");

    assert_eq!(plan.rows.len(), 2);
    assert_eq!(plan.rows[0].key().parts(), [KeyPart::UInt64(1)]);
    assert_eq!(plan.rows[1].key().parts(), [KeyPart::UInt64(2)]);
    assert_eq!(plan.rows[0].values()[0], Value::UInt64(1));
    assert_eq!(plan.rows[0].values()[1], Value::Utf8("first".to_owned()));
    assert_eq!(plan.rows[1].values()[2], Value::Null);
    // The store assigns the real commit version; binding must not invent one.
    assert_eq!(plan.rows[0].version(), 0);
}

#[test]
fn an_omitted_column_list_means_every_column_in_order() {
    let plan = insert("INSERT INTO notes VALUES (7, 'body', 'note')").expect("binds");
    assert_eq!(plan.rows[0].values()[0], Value::UInt64(7));
    assert_eq!(plan.rows[0].values()[2], Value::Utf8("note".to_owned()));
}

#[test]
fn a_duplicate_key_within_one_statement_is_1062() {
    let error = insert("INSERT INTO notes (id, body) VALUES (1, 'a'), (1, 'b')").expect_err("dup");
    assert_eq!(error.mysql_code(), 1062);
    assert_eq!(error.sqlstate(), "23000");
    assert!(error.to_string().contains("Duplicate entry '1'"), "{error}");
}

#[test]
fn a_null_in_a_not_null_column_is_1048() {
    // Both spellings: an explicit NULL, and a column simply left out.
    for sql in [
        "INSERT INTO notes (id, body) VALUES (1, NULL)",
        "INSERT INTO notes (id) VALUES (1)",
    ] {
        let error = insert(sql).unwrap_err();
        assert_eq!(error.mysql_code(), 1048, "for {sql}");
        assert!(error.to_string().contains("body"), "{error}");
    }
}

#[test]
fn an_unknown_column_is_1054_and_an_unknown_table_is_1146() {
    let unknown_column = insert("INSERT INTO notes (id, nope) VALUES (1, 'x')").unwrap_err();
    assert_eq!(unknown_column.mysql_code(), 1054);

    let unknown_table = insert("INSERT INTO elsewhere (id) VALUES (1)").unwrap_err();
    assert_eq!(unknown_table.mysql_code(), 1146);
}

#[test]
fn a_value_count_mismatch_is_refused() {
    let error = insert("INSERT INTO notes (id, body) VALUES (1)").unwrap_err();
    assert!(error.to_string().contains("Column count"), "{error}");
}

#[test]
fn a_narrow_column_refuses_a_value_it_cannot_hold() {
    // The schema only checks the PHYSICAL variant, so without an explicit
    // width check a TINYINT would silently store 99999 - a row no mirrored
    // table could contain.
    let table = create("CREATE TABLE t (id BIGINT PRIMARY KEY, small TINYINT)")
        .expect("binds")
        .table;
    let error = bind_insert(
        &parse_statement("INSERT INTO t (id, small) VALUES (1, 99999)").expect("parses"),
        &table,
    )
    .expect_err("refused");
    assert!(error.to_string().contains("Out of range"), "{error}");

    // The same column accepts a value that fits.
    assert!(
        bind_insert(
            &parse_statement("INSERT INTO t (id, small) VALUES (1, 127)").expect("parses"),
            &table,
        )
        .is_ok()
    );
}

#[test]
fn out_of_scope_insert_features_are_refused() {
    for sql in [
        "INSERT IGNORE INTO notes (id, body) VALUES (1, 'a')",
        "REPLACE INTO notes (id, body) VALUES (1, 'a')",
        "INSERT INTO notes (id, body) VALUES (1, 'a') ON DUPLICATE KEY UPDATE body = 'b'",
        "INSERT INTO notes (id, body) SELECT 1, 'a'",
    ] {
        assert!(insert(sql).is_err(), "must refuse: {sql}");
    }
}

#[test]
fn an_expression_value_is_refused_rather_than_silently_wrong() {
    // Binding takes literals only. Accepting `1 + 1` here without an
    // evaluator would store the text rather than the sum.
    let error = insert("INSERT INTO notes (id, body) VALUES (1 + 1, 'a')").unwrap_err();
    assert_eq!(error.mysql_code(), 1064);
}
