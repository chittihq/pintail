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
/// Few enough that the join can be run pair by pair: run as written it
/// holds every pair with both labels, and spills what the budget cannot.
const FEW_ROWS: u64 = 600;
/// Rows a block of the table holds. A scan decodes a block at once, so a
/// budget has to hold one whatever path the join takes: blocks are kept
/// far smaller than the index, which holds every row.
const BLOCK_ROWS: usize = 64;

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

/// What `SELECT COUNT(*) ... ON a.label <> b.label` over `rows` rows
/// answers under `limit` bytes: the count, the pairs counted rather than
/// run, and the query's peak.
fn count_unequal_pairs(rows: u64, limit: usize) -> Result<(Vec<Value>, u64, usize), String> {
    let directory = tempfile::tempdir().expect("directory");
    let mut table = TableStore::open(
        directory.path(),
        schema(),
        StoreOptions {
            block_rows: BLOCK_ROWS,
            ..StoreOptions::default()
        },
    )
    .expect("table");
    table
        .bulk_ingest_snapshot(
            (0..rows)
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
        TableStatistics::with_row_count(rows),
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
    let mut execution = Execution::start(plan, &provider, limit, Collation::default())
        .map_err(|error| error.to_string())?;
    let mut answer = Vec::new();
    while let Some(batch) = execution.next_batch().map_err(|error| error.to_string())? {
        for row in batch.selection().selected_rows() {
            answer.push(batch.columns()[0].value_owned(row).expect("value"));
        }
    }
    Ok((
        answer,
        take_exec_counters().join_pairs_counted,
        execution.memory().peak(),
    ))
}

#[test]
fn a_counted_index_is_charged_to_the_query() {
    let (answer, counted, peak) = count_unequal_pairs(ROWS, 1 << 30).expect("runs");
    let pairs = ROWS * (ROWS - 1);
    assert_eq!(answer, vec![Value::UInt64(pairs)]);
    assert_eq!(counted, pairs, "counted");
    // Every right label is a key the index holds at once.
    let index = usize::try_from(ROWS).expect("small") * LABEL_BYTES;
    assert!(
        peak >= index,
        "peak {peak} bytes is under the {index} bytes the index holds"
    );
}

/// No budget gives another answer: with room the join is counted, and
/// under less the query answers the same or is refused for memory. The
/// table's own scans need more than the index does, so a budget that fits
/// them and not the index is too narrow a band to pin here.
#[test]
fn no_budget_changes_a_counted_joins_answer() {
    let pairs = FEW_ROWS * (FEW_ROWS - 1);
    let (_, counted, roomy) = count_unequal_pairs(FEW_ROWS, 1 << 30).expect("runs");
    assert_eq!(counted, pairs, "counted with room");
    let mut answered = 0;
    for step in (2..=10).rev() {
        let limit = roomy * step / 2;
        match count_unequal_pairs(FEW_ROWS, limit) {
            Ok((answer, counted, peak)) => {
                assert_eq!(answer, vec![Value::UInt64(pairs)], "under {limit} bytes");
                assert!(peak <= limit, "peak {peak} bytes is over {limit}");
                assert!(counted == 0 || counted == pairs, "{counted} pairs counted");
                answered += 1;
            }
            Err(error) => {
                assert!(
                    error.contains("memory limit"),
                    "under {limit} bytes: {error}"
                );
            }
        }
    }
    assert!(answered > 0, "no budget answered");
}
