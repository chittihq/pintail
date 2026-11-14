//! A row compared with a subquery that aggregates without GROUP BY: the
//! subquery is one row whatever it reads, an empty table included, and a
//! column of it read on its own - a constant beside `COUNT(*)` - is still
//! read from that one row.

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
            Column::new(2, "grp", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

/// The first value of the one row `sql` answers, over `items` with four
/// rows in two groups of two, and `empty` with none.
fn single(sql: &str) -> Value {
    let directory = tempfile::tempdir().expect("directory");
    let open = |name: &str, rows: u64| {
        let mut table = TableStore::open(
            directory.path().join(name),
            schema(),
            StoreOptions::default(),
        )
        .expect("table");
        table
            .bulk_ingest_snapshot(
                (0..rows)
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            vec![
                                Value::UInt64(id),
                                Value::Int64(i64::try_from(id % 2).expect("small")),
                            ],
                            1,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("ingest");
        table
    };
    let items = open("items", 4);
    let empty = open("empty", 0);
    let (items_snapshot, empty_snapshot) = (items.snapshot(), empty.snapshot());
    let entry = |id: u64, name: &str, rows: u64| {
        TableEntry::new(
            TableId::new(id),
            name,
            schema(),
            TableStatistics::with_row_count(rows),
        )
        .expect("entry")
    };
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        DatabaseId::new(1),
        "app",
        [entry(1, "items", 4), entry(2, "empty", 0)],
    )
    .expect("database")])
    .expect("catalog");
    let provider = SnapshotScanProvider::new([
        (DatabaseId::new(1), TableId::new(1), &items_snapshot),
        (DatabaseId::new(1), TableId::new(2), &empty_snapshot),
    ])
    .expect("provider");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(plan, &provider, 64 << 20, Collation::default()).expect("execution");
    let mut answer = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            answer.push(batch.columns()[0].value_owned(row).expect("value"));
        }
    }
    assert_eq!(answer.len(), 1, "{sql}");
    answer.remove(0)
}

#[test]
fn an_aggregate_row_subquery_is_one_row() {
    let cases = [
        // Each outer row compares with the subquery's one row.
        "SELECT COUNT(*) FROM items o WHERE (4, 7) = (SELECT COUNT(*), 7 FROM items)",
        // An empty table aggregates to one row too: COUNT(*) is 0 and the
        // constant is still 7.
        "SELECT COUNT(*) FROM items o WHERE (0, 7) = (SELECT COUNT(*), 7 FROM empty)",
        // Correlated: per outer row, the two rows of its group.
        "SELECT COUNT(*) FROM items o \
         WHERE (2, 7) = (SELECT COUNT(*), 7 FROM items i WHERE i.grp = o.grp)",
        // No table at all: the columns are read as written, with no HAVING.
        "SELECT COUNT(*) FROM items o WHERE (4, 7) = (SELECT 4, 7)",
        // No table at all: one row, whose count is 1.
        "SELECT COUNT(*) FROM items o WHERE (1, 7) = (SELECT COUNT(*), 7)",
        // The same read as a scalar: an aggregate over the one row.
        "SELECT COUNT(*) FROM items o WHERE 3 = (SELECT SUM(2) + COUNT(*))",
        // Every column an aggregate: one row as before.
        "SELECT COUNT(*) FROM items o WHERE (4, 3) = (SELECT COUNT(*), MAX(id) FROM items)",
    ];
    for sql in cases {
        assert_eq!(single(sql), Value::UInt64(4), "{sql}");
    }
}
