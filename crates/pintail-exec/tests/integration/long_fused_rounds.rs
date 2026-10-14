//! A scan folded in place reads a short first round and, when that round
//! handed no batch back, long ones after it: the table below has several
//! first rounds' worth of slices and is read in long rounds after the
//! first, to the same answer.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS_PER_SEGMENT: u64 = 50;

#[test]
fn a_scan_folded_in_place_runs_long_rounds_after_a_first_short_one() {
    if std::env::var_os("PINTAIL_DISABLE_FUSED_FOLD").is_some() {
        return;
    }
    // Twelve slices a thread: three first rounds' worth.
    let segments = u64::try_from(rayon::current_num_threads().max(1) * 12).expect("small");
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "n", DataType::Int64, false),
        ],
    )
    .expect("schema");
    let directory = tempfile::tempdir().expect("directory");
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema.clone(), options).expect("table");
    for segment in 0..segments {
        table
            .ingest(
                (segment * ROWS_PER_SEGMENT..(segment + 1) * ROWS_PER_SEGMENT)
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            vec![
                                Value::UInt64(id),
                                Value::Int64(i64::try_from(id % 7).expect("small")),
                            ],
                            1,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("ingest");
        table.flush().expect("flush");
    }
    let rows = segments * ROWS_PER_SEGMENT;
    let entry = TableEntry::new(
        TableId::new(1),
        "readings",
        schema,
        TableStatistics::with_row_count(rows),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let sql = "SELECT COUNT(*) FROM readings WHERE n < 5";
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let _ = pintail_exec::take_exec_counters();
    let mut execution =
        Execution::start(plan, &provider, 256 << 20, Collation::default()).expect("execution");
    let mut answer = Vec::new();
    while let Some(batch) = execution.next_batch().expect("batch") {
        for row in batch.selection().selected_rows() {
            answer.push(match batch.columns()[0].value(row).expect("value") {
                Value::UInt64(count) => *count,
                Value::Int64(count) => u64::try_from(*count).expect("a count"),
                other => panic!("unexpected {other:?}"),
            });
        }
    }
    let expected = (0..rows).filter(|id| id % 7 < 5).count() as u64;
    assert_eq!(answer, [expected]);
    let counters = pintail_exec::take_exec_counters();
    // A first round, then the rest of the table in one - unless another
    // scan in this process was folding too, which cuts a long round short.
    assert!(
        counters.fused_rounds >= 2 && counters.fused_long_rounds >= 1,
        "{counters:?}"
    );
}
