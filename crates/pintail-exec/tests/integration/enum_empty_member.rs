//! `ENUM('', 'zz', 'aa')` is legal `MySQL` and `''` there is a REAL member
//! at ordinal 1, distinct from the error value (ordinal 0). Sorting and
//! grouping follow the declaration index, so the empty member sorts FIRST
//! and `zz` sorts before `aa`. Every expectation here is `MySQL` 8.4's
//! answer for this data, measured on the live pair. The trap this pins:
//! a batch-repack label table keeps unseen slots as empty strings, and an
//! empty-label guard broad enough to cover those gaps silently demotes the
//! declared `''` member to plain text — alphabetical order — instead.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Declaration order: ''(1), zz(2), aa(3). Alphabetical order would put
/// `aa` before `zz`, so text ordering cannot pass by luck.
const LABELS: [&str; 3] = ["", "zz", "aa"];

/// Counts: '' twice, zz twice, aa once — matching the live-pair probe.
const ROWS: [(u64, &str); 5] = [(1, "zz"), (2, ""), (3, "aa"), (4, "zz"), (5, "")];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "v", DataType::Utf8, false)
                .with_enum_labels(Some(LABELS.iter().map(ToString::to_string).collect())),
        ],
    )
    .expect("schema")
}

fn row(id: u64, label: &str) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Utf8(label.to_owned())],
        id,
        false,
    )
}

fn run(sql: &str, split: bool) -> Vec<Vec<String>> {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    let build = |(id, label): &(u64, &str)| row(*id, label);
    if split {
        // Tail rows arrive through the WAL/memtable path the CDC stream
        // uses — the snapshot/memtable split that has disagreed before
        // (#256). The empty member lands on BOTH sides.
        table
            .bulk_ingest_snapshot(ROWS[..3].iter().map(build).collect())
            .expect("snapshot ingest");
        table
            .ingest(ROWS[3..].iter().map(build).collect())
            .expect("memtable ingest");
    } else {
        table
            .bulk_ingest_snapshot(ROWS.iter().map(build).collect())
            .expect("snapshot ingest");
    }
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(1);
    let table_id = TableId::new(1);
    let entry = TableEntry::new(
        table_id,
        "e",
        schema(),
        TableStatistics::with_row_count(ROWS.len() as u64),
    )
    .expect("entry");
    let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("execute") {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value(row) {
                        Some(Value::Null) | None => "NULL".to_owned(),
                        Some(Value::Utf8(text) | Value::Enum { label: text, .. }) => text.clone(),
                        Some(Value::UInt64(number)) => number.to_string(),
                        Some(Value::Int64(number)) => number.to_string(),
                        Some(other) => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>(),
            );
        }
    }
    rows
}

/// `MySQL` 8.4: `SELECT id FROM e ORDER BY v, id` → 2, 5, 1, 4, 3 — the
/// empty member (ordinal 1) first, then zz (2), then aa (3).
#[test]
fn order_by_puts_the_empty_member_first_and_zz_before_aa() {
    for split in [false, true] {
        let rows = run("SELECT id FROM e ORDER BY v, id", split);
        assert_eq!(
            rows.iter().map(|row| row[0].as_str()).collect::<Vec<_>>(),
            ["2", "5", "1", "4", "3"],
            "split={split}"
        );
    }
}

/// `MySQL` 8.4: `SELECT v, COUNT(*) FROM e GROUP BY v ORDER BY v` →
/// ('', 2), ('zz', 2), ('aa', 1) — declaration order, not alphabetical.
#[test]
fn grouping_orders_the_empty_member_by_its_ordinal() {
    for split in [false, true] {
        let rows = run("SELECT v, COUNT(*) FROM e GROUP BY v ORDER BY v", split);
        assert_eq!(rows, [["", "2"], ["zz", "2"], ["aa", "1"]], "split={split}");
    }
}

/// `MySQL` 8.4: DISTINCT walks the declaration order the same way.
#[test]
fn distinct_orders_the_empty_member_by_its_ordinal() {
    for split in [false, true] {
        let rows = run("SELECT DISTINCT v FROM e ORDER BY v", split);
        assert_eq!(
            rows.iter().map(|row| row[0].as_str()).collect::<Vec<_>>(),
            ["", "zz", "aa"],
            "split={split}"
        );
    }
}

/// Equality still matches by text: the empty member is selectable.
#[test]
fn equality_matches_the_empty_member_by_text() {
    for split in [false, true] {
        let rows = run("SELECT COUNT(*) FROM e WHERE v = ''", split);
        assert_eq!(rows, [["2"]], "split={split}");
    }
}
