//! An expression that leaves its type's range fails with `MySQL`'s message,
//! which names the innermost expression that overflowed as `MySQL` prints
//! it; and each division by zero answered with NULL is counted, since
//! `MySQL` raises one warning per occurrence.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "u", DataType::UInt64, false),
        ],
    )
    .expect("schema")
}

/// The rows `sql` answers, or its error, and the divisions by zero it
/// counted.
fn run(sql: &str) -> (Result<usize, String>, u64) {
    let directory = tempfile::tempdir().expect("directory");
    let mut store =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("store");
    store
        .bulk_ingest_snapshot(
            [(1_u64, 0_u64), (2, 7)]
                .into_iter()
                .map(|(id, u)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![Value::UInt64(id), Value::UInt64(u)],
                        id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let entry = TableEntry::new(
        TableId::new(1),
        "amounts",
        schema(),
        TableStatistics::with_row_count(2),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let snapshot = store.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    pintail_sql::set_session_database_name(Some("app"));
    let _ = pintail_exec::take_session_division_warnings();
    let result = (|| {
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .map_err(|error| error.to_string())?;
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .map_err(|error| error.to_string())?;
        let mut execution =
            Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
                .map_err(|error| error.to_string())?;
        let mut rows = 0;
        while let Some(batch) = execution.next_batch().map_err(|error| error.to_string())? {
            rows += batch.visible_row_count();
        }
        Ok(rows)
    })();
    (result, pintail_exec::take_session_division_warnings())
}

#[test]
fn an_overflow_names_the_expression_as_mysql_prints_it() {
    for (sql, message) in [
        (
            "SELECT CAST(0 AS UNSIGNED) - 1",
            "BIGINT UNSIGNED value is out of range in '(cast(0 as unsigned) - 1)'",
        ),
        (
            "SELECT u - 5 FROM amounts WHERE id = 1",
            "BIGINT UNSIGNED value is out of range in '(`app`.`amounts`.`u` - 5)'",
        ),
        (
            "SELECT (u - 5) * 2 FROM amounts WHERE id = 1",
            "BIGINT UNSIGNED value is out of range in '(`app`.`amounts`.`u` - 5)'",
        ),
        (
            "SELECT 9223372036854775807 + 1",
            "BIGINT value is out of range in '(9223372036854775807 + 1)'",
        ),
    ] {
        assert_eq!(run(sql).0, Err(message.to_owned()), "{sql}");
    }
}

#[test]
fn each_division_by_zero_is_counted_once() {
    assert_eq!(run("SELECT 1/0"), (Ok(1), 1));
    assert_eq!(run("SELECT id / 0 FROM amounts"), (Ok(2), 2));
    assert_eq!(run("SELECT 5 % 0, 5 DIV 0"), (Ok(1), 2));
    assert_eq!(run("SELECT id / 2 FROM amounts"), (Ok(2), 0));
}
