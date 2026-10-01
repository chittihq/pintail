//! A table that lives wholly in the memtable, with a wide text column, must
//! answer under a tight per-query ceiling by spilling. Its rows reach the
//! operators packed, so a join's build that used to spill now fits and the
//! aggregate above it works in what the build leaves.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const TABLE_ID: TableId = TableId::new(1);
const ROWS: u64 = 120_000;
const GROUPS: u64 = 15_000;
const MIB: usize = 1024 * 1024;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "k", DataType::Int64, false),
            Column::new(3, "payload", DataType::Utf8, false),
            Column::new(
                4,
                "amount",
                DataType::Decimal {
                    precision: 12,
                    scale: 3,
                },
                false,
            ),
        ],
    )
    .expect("schema")
}

fn row(id: u64, groups: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(i64::try_from(id % groups).expect("group")),
            Value::Utf8(format!("{id:06}{}", "x".repeat(234))),
            Value::Utf8(format!("0.{:03}", id % 1000)),
        ],
        id,
        false,
    )
}

/// Runs `sql` under `limit` and returns its rows and spill files, or the
/// error text.
fn run(table: &TableStore, limit: usize, sql: &str) -> Result<(Vec<Vec<Value>>, u64), String> {
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TABLE_ID,
        "wide_rows",
        schema(),
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog =
        CatalogSnapshot::new([DatabaseEntry::new(DATABASE_ID, "app", [entry]).expect("database")])
            .expect("catalog");
    let provider =
        SnapshotScanProvider::new([(DATABASE_ID, TABLE_ID, &snapshot)]).expect("provider");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution = Execution::start(plan, &provider, limit, Collation::default())
        .map_err(|error| error.to_string())?;
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().map_err(|error| error.to_string())? {
        for index in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| column.value(index).cloned().expect("value"))
                    .collect(),
            );
        }
    }
    Ok((rows, execution.spill_metrics().files))
}

/// The rows, held in the memtable, with `groups` values of `k`.
fn table(groups: u64) -> (tempfile::TempDir, TableStore) {
    let directory = tempfile::tempdir().expect("dir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    for chunk in (0..ROWS).collect::<Vec<_>>().chunks(10_000) {
        table
            .ingest(chunk.iter().map(|id| row(*id, groups)).collect())
            .expect("ingest");
    }
    (directory, table)
}

const CASES: [(&str, &str, u64); 4] = [
    (
        "aggregate",
        "SELECT k,COUNT(*),SUM(amount),MIN(payload),COUNT(DISTINCT payload) \
         FROM wide_rows GROUP BY k ORDER BY k",
        GROUPS,
    ),
    (
        "sort",
        "SELECT id FROM wide_rows ORDER BY payload DESC,id",
        ROWS,
    ),
    (
        "join",
        "SELECT a.k,COUNT(*),SUM(a.amount) FROM wide_rows a JOIN wide_rows b ON a.id=b.id \
         GROUP BY a.k ORDER BY a.k",
        GROUPS,
    ),
    (
        "window",
        "SELECT id,ROW_NUMBER() OVER (PARTITION BY k ORDER BY id) FROM wide_rows ORDER BY id",
        ROWS,
    ),
];

#[test]
fn a_wide_memtable_table_answers_under_every_ceiling() {
    let (_directory, table) = table(GROUPS);
    let mut failures = Vec::new();
    for (family, sql, expected) in CASES {
        let (reference, _) = run(&table, 256 * MIB, sql).expect("roomy run");
        assert_eq!(
            reference.len(),
            usize::try_from(expected).expect("rows"),
            "{family}"
        );
        // The join's build fits every ceiling here and leaves the aggregate
        // above it the remainder: at 12 MiB that is less than its groups
        // take, which is where the grouped fold has to go to disk.
        for limit in [12 * MIB, 16 * MIB, 24 * MIB, 64 * MIB] {
            match run(&table, limit, sql) {
                Ok((rows, files)) => {
                    println!("{family} at {} MiB: {files} spill files", limit / MIB);
                    if rows != reference {
                        failures.push(format!("{family} at {limit}: rows differ"));
                    }
                    if limit == 12 * MIB && files == 0 {
                        failures.push(format!("{family} at {limit}: nothing spilled"));
                    }
                }
                Err(error) => failures.push(format!("{family} at {limit}: {error}")),
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A dense integer key folds into plain cells, a slab per worker, and the
/// groups cost more once they are handed to the maps a spill writes from.
/// Across these ceilings the slabs alone pass half of the budget somewhere
/// along the input, so the hand-over meets a budget that cannot take every
/// group: it has to write what it has moved and carry on.
#[test]
fn a_dense_integer_key_spills_when_its_groups_outgrow_the_ceiling() {
    const MANY: u64 = 40_000;
    let (_directory, table) = table(MANY);
    let sql = "SELECT k,COUNT(*),SUM(amount) FROM wide_rows GROUP BY k ORDER BY k";
    let (reference, _) = run(&table, 256 * MIB, sql).expect("roomy run");
    assert_eq!(reference.len(), usize::try_from(MANY).expect("groups"));
    let mut failures = Vec::new();
    for limit in (4..=32).step_by(2).map(|mib| mib * MIB) {
        match run(&table, limit, sql) {
            Ok((rows, files)) => {
                println!("dense key at {} MiB: {files} spill files", limit / MIB);
                if rows != reference {
                    failures.push(format!("{limit}: rows differ"));
                }
            }
            Err(error) => failures.push(format!("{limit}: {error}")),
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
