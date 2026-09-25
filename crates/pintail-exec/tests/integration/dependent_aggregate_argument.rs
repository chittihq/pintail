//! An aggregate whose argument holds a correlated subquery -
//! `SUM(open AND EXISTS (SELECT ... WHERE c.shop_id = s.shop_id))` - is
//! answered by resolving the subquery per input row before aggregating.
//! Compiled expressions cannot run a subquery, and the argument used to
//! reach compilation unresolved and fail the statement as an invalid plan.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn shops() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "shop_id", DataType::UInt64, false),
            Column::new(2, "open", DataType::Int64, false),
        ],
    )
    .expect("shop schema")
}

fn counters() -> TableSchema {
    TableSchema::new(
        2,
        vec![
            Column::new(1, "counter_id", DataType::UInt64, false),
            Column::new(2, "shop_id", DataType::UInt64, false),
        ],
    )
    .expect("counter schema")
}

fn row(id: u64, values: Vec<Value>) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        values,
        id,
        false,
    )
}

fn table(directory: &std::path::Path, schema: TableSchema, rows: Vec<StoredRow>) -> TableStore {
    let mut store = TableStore::open(directory, schema, StoreOptions::default()).expect("open");
    store.bulk_ingest_snapshot(rows).expect("rows");
    store.flush().expect("flush");
    store
}

#[test]
fn a_correlated_subquery_inside_an_aggregate_argument_is_answered() {
    let directory = tempfile::tempdir().expect("temporary tables");
    // Shops 1 and 3 are open; shops 1 and 2 have counters, shop 1 has two.
    let shop_store = table(
        &directory.path().join("shops"),
        shops(),
        [(1, 1), (2, 0), (3, 1)]
            .into_iter()
            .map(|(id, open)| row(id, vec![Value::UInt64(id), Value::Int64(open)]))
            .collect(),
    );
    let counter_store = table(
        &directory.path().join("counters"),
        counters(),
        [(10, 1), (11, 1), (12, 2)]
            .into_iter()
            .map(|(id, shop)| row(id, vec![Value::UInt64(id), Value::UInt64(shop)]))
            .collect(),
    );
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        DatabaseId::new(3),
        "app",
        [
            TableEntry::new(
                TableId::new(1),
                "shops",
                shops(),
                TableStatistics::with_row_count(3),
            )
            .expect("shops"),
            TableEntry::new(
                TableId::new(2),
                "counters",
                counters(),
                TableStatistics::with_row_count(3),
            )
            .expect("counters"),
        ],
    )
    .expect("database")])
    .expect("catalog");
    let (shop_snapshot, counter_snapshot) = (shop_store.snapshot(), counter_store.snapshot());
    let provider = SnapshotScanProvider::new([
        (DatabaseId::new(3), TableId::new(1), &shop_snapshot),
        (DatabaseId::new(3), TableId::new(2), &counter_snapshot),
    ])
    .expect("provider");
    let answer = |sql: &str| -> Vec<Value> {
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
                .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let batch = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
            .expect("one row");
        (0..batch.columns().len())
            .map(|index| {
                batch
                    .column(index)
                    .and_then(|column| column.value(0))
                    .cloned()
                    .expect("value")
            })
            .collect()
    };
    let text = |values: Vec<Value>| {
        values
            .iter()
            .map(|value| format!("{value:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    };

    assert_eq!(
        text(answer(
            "SELECT COUNT(*), SUM(s.open = 1 AND EXISTS (SELECT 1 FROM counters c WHERE c.shop_id = s.shop_id)) FROM shops s"
        )),
        text(vec![Value::UInt64(3), Value::Int64(1)]),
    );
    assert_eq!(
        text(answer(
            "SELECT COUNT(CASE WHEN (SELECT COUNT(*) FROM counters c WHERE c.shop_id = s.shop_id) > 1 THEN 1 END) FROM shops s"
        )),
        text(vec![Value::UInt64(1)]),
    );
}
