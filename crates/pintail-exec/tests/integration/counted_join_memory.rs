//! A join counted rather than run holds an index of its right side: a key
//! per distinct value for `<>`, a sorted value per row for a range. That
//! index is taken from the query's budget like any other held row, so a
//! wide one shows in the query's peak and a budget too small for it leaves
//! the join to its own path.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider, take_exec_counters,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 40_000;
const LABEL_BYTES: usize = 500;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

/// Every label distinct and `LABEL_BYTES` long.
fn label(id: u64) -> String {
    let mut label = format!("{id:08}-");
    label.extend(std::iter::repeat_n('x', LABEL_BYTES - label.len()));
    label
}

#[test]
fn a_counted_index_is_charged_to_the_query() {
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
    table
        .bulk_ingest_snapshot(
            (0..ROWS)
                .map(|id| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![Value::UInt64(id), Value::Utf8(label(id))],
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TableId::new(1),
        "labels",
        schema(),
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let sql = "SELECT COUNT(*) FROM labels a JOIN labels b ON a.label <> b.label";
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let _ = take_exec_counters();
    let mut execution =
        Execution::start(plan, &provider, 1 << 30, Collation::default()).expect("execution");
    let mut answer = Vec::new();
    while let Some(batch) = execution.next_batch().expect("batch") {
        for row in batch.selection().selected_rows() {
            answer.push(batch.columns()[0].value_owned(row).expect("value"));
        }
    }
    let pairs = ROWS * (ROWS - 1);
    assert_eq!(answer, vec![Value::UInt64(pairs)]);
    assert_eq!(take_exec_counters().join_pairs_counted, pairs, "counted");
    // Every right label is a key the index holds at once.
    let index = usize::try_from(ROWS).expect("small") * LABEL_BYTES;
    let peak = execution.memory().peak();
    assert!(
        peak >= index,
        "peak {peak} bytes is under the {index} bytes the index holds"
    );
}
