//! `MySQL`'s rounding family — ROUND, TRUNCATE, CEILING, FLOOR — reads a
//! computed decimal operand's INTERNAL digits, not its declared display
//! scale. A division advertised at scale 4 carries 9 truncated fractional
//! digits for its parent (base-1e9 decimal words), so ROUND(28100/508, 2)
//! is 55.31 from 55.314960629 while the bare division displays 55.3150.
//! Rounding the display value instead double-rounds; the stable-gate corpus
//! caught exactly that on `ROUND(COUNT(*) * 100 / SUM(COUNT(*)) OVER (), 2)`.
//! Every expectation here was measured on `MySQL` 8.4 on the live pair.

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
            Column::new(2, "n", DataType::Int64, false),
            Column::new(3, "d", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

/// One row is enough: the expressions under test are literal or single-row.
const ROWS: [(u64, i64, i64); 1] = [(1, 281, 508)];

fn run(sql: &str) -> Vec<Vec<String>> {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    table
        .bulk_ingest_snapshot(
            ROWS.iter()
                .map(|(id, n, d)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
                        vec![Value::UInt64(*id), Value::Int64(*n), Value::Int64(*d)],
                        *id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(1);
    let table_id = TableId::new(1);
    let entry = TableEntry::new(
        table_id,
        "t",
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
                        Some(Value::Utf8(text)) => text.clone(),
                        Some(Value::Int64(number)) => number.to_string(),
                        Some(Value::UInt64(number)) => number.to_string(),
                        Some(other) => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>(),
            );
        }
    }
    rows
}

fn one(sql: &str) -> Vec<String> {
    let rows = run(sql);
    assert_eq!(rows.len(), 1, "expected one row from {sql}");
    rows.into_iter().next().expect("one row")
}

#[test]
fn round_reads_the_divisions_internal_digits() {
    // 28100/508 = 55.314960629... — displayed 55.3150, but ROUND sees the
    // internal digits: 55.31, never the double-rounded 55.32.
    assert_eq!(one("SELECT ROUND(28100/508, 2) FROM t"), ["55.31"]);
}

#[test]
fn the_gate_shape_rounds_from_internal_digits() {
    // The corpus query that failed the stable gate, reduced: the dividend
    // and divisor arrive from COLUMNS, through a multiply, as in
    // ROUND(COUNT(*) * 100 / SUM(COUNT(*)) OVER (), 2).
    assert_eq!(one("SELECT ROUND(n * 100 / d, 2) FROM t"), ["55.31"]);
}

#[test]
fn bare_division_still_displays_the_declared_scale() {
    assert_eq!(one("SELECT 28100/508 FROM t"), ["55.3150"]);
}

#[test]
fn round_result_scale_caps_at_the_arguments_declared_scale() {
    // ROUND(1/3, 6): the division's declared scale is 4, so the RESULT
    // renders 4 digits even though 6 were requested.
    assert_eq!(one("SELECT ROUND(1/3, 6) FROM t"), ["0.3333"]);
}

#[test]
fn round_applies_the_effective_digits_once() {
    // min(d, declared) applied ONCE to the internal value — not round-at-6
    // then re-round-at-4 (33334999/1E8 would answer 0.3334 if it were).
    assert_eq!(
        one("SELECT ROUND(33334999/100000000, 6) FROM t"),
        ["0.3333"]
    );
    assert_eq!(
        one("SELECT ROUND(33335001/100000000, 6) FROM t"),
        ["0.3334"]
    );
}

#[test]
fn truncate_reads_internal_digits_and_caps_at_declared_scale() {
    // Internal 55.314960629: truncating at 3 keeps 55.314 (the display
    // value 55.3150 would give 55.315); at 9 the declared scale-4 cap
    // truncates to 55.3149; TRUNCATE(1/3, 6) caps the same way.
    assert_eq!(one("SELECT TRUNCATE(28100/508, 3) FROM t"), ["55.314"]);
    assert_eq!(one("SELECT TRUNCATE(28100/508, 9) FROM t"), ["55.3149"]);
    assert_eq!(one("SELECT TRUNCATE(1/3, 6) FROM t"), ["0.3333"]);
}

#[test]
fn exact_quotients_still_pad_to_the_result_scale() {
    // The internal value being EXACT must not shrink the render: MySQL pads
    // ROUND/TRUNCATE results to min(declared scale, digits) — the oracle
    // caught 4.00 collapsing to 4 when the chain materialized minimally.
    assert_eq!(one("SELECT ROUND(400/100, 2) FROM t"), ["4.00"]);
    assert_eq!(one("SELECT TRUNCATE(70/7, 2) FROM t"), ["10.00"]);
    assert_eq!(one("SELECT ROUND(10.50/3, 4) FROM t"), ["3.5000"]);
    assert_eq!(one("SELECT TRUNCATE(10.50/3, 6) FROM t"), ["3.500000"]);
    assert_eq!(one("SELECT ROUND(1/4, 6) FROM t"), ["0.2500"]);
}

#[test]
fn floor_and_ceiling_read_internal_digits() {
    // 39999.6/10000 is internally 3.99996 (displayed 4.00000): FLOOR is 3.
    // 40000.4/10000 is internally 4.00004: CEILING is 5.
    assert_eq!(one("SELECT FLOOR(39999.6/10000) FROM t"), ["3"]);
    assert_eq!(one("SELECT CEILING(40000.4/10000) FROM t"), ["5"]);
}

#[test]
fn negative_digits_round_exact_half_ties_away_from_zero() {
    // MySQL exact DECIMAL rounding is half-away-from-zero even left of the
    // decimal point. The generated MySQL corpus found the computed-expression
    // shape: ROUND(50.00 + 0.00, -2) is 100, never nearest-even 0.
    assert_eq!(
        one("SELECT ROUND(CAST(50.00 AS DECIMAL(4,2)) + 0.00, -2) FROM t"),
        ["100"]
    );
    assert_eq!(
        one("SELECT ROUND(CAST(-50.00 AS DECIMAL(4,2)) + 0.00, -2) FROM t"),
        ["-100"]
    );
    assert_eq!(
        one("SELECT TRUNCATE(CAST(50.00 AS DECIMAL(4,2)) + 0.00, -2) FROM t"),
        ["0"]
    );
}
