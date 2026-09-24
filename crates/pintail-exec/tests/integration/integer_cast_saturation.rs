//! An explicit `CAST` to an integer answers as `MySQL` 8.4 does: an operand
//! that does not fit saturates instead of refusing with a numeric overflow,
//! and a fractional number rounds - a float half to even, a decimal half
//! away from zero - instead of truncating. Every expected value below was
//! read from `MySQL` 8.4.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, TableSchema, Value};

fn answer(sql: &str) -> Value {
    let schema =
        TableSchema::new(1, vec![Column::new(1, "id", DataType::UInt64, false)]).expect("schema");
    let directory = tempfile::tempdir().expect("directory");
    let table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).expect("table");
    let entry = TableEntry::new(
        TableId::new(1),
        "t",
        schema,
        TableStatistics::with_row_count(0),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog =
        CatalogSnapshot::new([DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("db")])
            .expect("catalog");
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 1 << 26, Collation::default()).expect("start");
    let batch = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .expect("one row");
    let row = batch.selection().selected_rows().next().expect("a row");
    batch
        .column(0)
        .and_then(|column| column.value(row))
        .cloned()
        .expect("value")
}

#[test]
fn an_integer_cast_that_does_not_fit_answers_as_mysql_does() {
    for (sql, expected) in [
        // Text reads its integer and saturates to the unsigned range; SIGNED
        // then reinterprets the bits.
        (
            "SELECT CAST('18446744073709551616' AS UNSIGNED)",
            Value::UInt64(u64::MAX),
        ),
        (
            "SELECT CAST('18446744073709551616' AS SIGNED)",
            Value::Int64(-1),
        ),
        (
            "SELECT CAST('18446744073709551615' AS SIGNED)",
            Value::Int64(-1),
        ),
        (
            "SELECT CAST('9223372036854775808' AS SIGNED)",
            Value::Int64(i64::MIN),
        ),
        (
            "SELECT CAST(CONCAT('184467440', '73709551615') AS SIGNED)",
            Value::Int64(-1),
        ),
        (
            "SELECT CAST('-9223372036854775809' AS SIGNED)",
            Value::Int64(i64::MIN),
        ),
        (
            "SELECT CAST('-99999999999999999999' AS UNSIGNED)",
            Value::UInt64(9_223_372_036_854_775_808),
        ),
        // A number saturates to the signed range; an exact decimal reaches
        // the top of the unsigned range under UNSIGNED.
        (
            "SELECT CAST(18446744073709551616 AS SIGNED)",
            Value::Int64(i64::MAX),
        ),
        (
            "SELECT CAST(18446744073709551616 AS UNSIGNED)",
            Value::UInt64(u64::MAX),
        ),
        (
            "SELECT CAST(-9223372036854775809 AS SIGNED)",
            Value::Int64(i64::MIN),
        ),
        (
            "SELECT CAST(-9223372036854775809 AS UNSIGNED)",
            Value::UInt64(9_223_372_036_854_775_808),
        ),
        (
            "SELECT CAST(99999999999999999999.5 AS SIGNED)",
            Value::Int64(i64::MAX),
        ),
        ("SELECT CAST(1.5e19 AS SIGNED)", Value::Int64(i64::MAX)),
        (
            "SELECT CAST(1.5e19 AS UNSIGNED)",
            Value::UInt64(i64::MAX.cast_unsigned()),
        ),
        (
            "SELECT CAST(1e20 AS UNSIGNED)",
            Value::UInt64(i64::MAX.cast_unsigned()),
        ),
        // What already fit keeps its answer.
        (
            "SELECT CAST(18446744073709551615 AS SIGNED)",
            Value::Int64(-1),
        ),
        ("SELECT CAST(-1 AS UNSIGNED)", Value::UInt64(u64::MAX)),
        ("SELECT CAST(-1.5e0 AS SIGNED)", Value::Int64(-2)),
        ("SELECT CAST(1.5e0 AS SIGNED)", Value::Int64(2)),
        ("SELECT CAST(2.5e0 AS SIGNED)", Value::Int64(2)),
        ("SELECT CAST(-2.5e0 AS SIGNED)", Value::Int64(-2)),
        ("SELECT CAST(0.5e0 AS SIGNED)", Value::Int64(0)),
        ("SELECT CAST(-0.5e0 AS SIGNED)", Value::Int64(0)),
        ("SELECT CAST(-1.5 AS SIGNED)", Value::Int64(-2)),
        ("SELECT CAST(2.5 AS SIGNED)", Value::Int64(3)),
        ("SELECT CAST('2.5' AS SIGNED)", Value::Int64(2)),
        ("SELECT CAST('-1.5' AS SIGNED)", Value::Int64(-1)),
        (
            "SELECT CAST(-1.5e0 AS UNSIGNED)",
            Value::UInt64(18_446_744_073_709_551_614),
        ),
        (
            "SELECT CAST(1e19 AS UNSIGNED)",
            Value::UInt64(i64::MAX.cast_unsigned()),
        ),
        (
            "SELECT CAST(9.2e18 AS UNSIGNED)",
            Value::UInt64(9_200_000_000_000_000_000),
        ),
        ("SELECT CAST(1.5e0 AS UNSIGNED)", Value::UInt64(2)),
    ] {
        assert_eq!(answer(sql), expected, "{sql}");
    }
}
