//! DISTINCT aggregates that key a DECIMAL column by its packed units, read
//! back after their groups spill: a `SUM` or an `AVG` adds the value each
//! key stands for, and a key past 64 bits - a DECIMAL of more than nineteen
//! digits - travels as that value's text. Beside them, a DISTINCT sum over
//! text, whose set keeps the value it first met.

use std::collections::{BTreeMap, BTreeSet};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 60_000;
const BUCKETS: u64 = 9_000;
/// What a wide amount's units are a multiple of, so they pass 64 bits.
const WIDE: i128 = 10_000_000_000_000_000_000_000;

fn mix(id: u64) -> u64 {
    let mut value = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    value ^= value >> 29;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^ (value >> 32)
}

fn bucket(id: u64) -> u64 {
    1 + (mix(id) >> 13) % BUCKETS
}

/// Hundredths, NULL for some rows.
fn amount(id: u64) -> Option<i64> {
    let hash = mix(id);
    (!(hash >> 3).is_multiple_of(10))
        .then(|| i64::try_from((hash >> 27) % 40).expect("small") * 25 - 300)
}

/// The amount's hundredths moved past 64 bits, keeping its sign.
fn wide(id: u64) -> Option<i128> {
    amount(id).map(|amount| i128::from(amount) * WIDE + i128::from(amount.signum()) * 7)
}

fn hundredths(units: i128) -> String {
    format!(
        "{}{}.{:02}",
        if units < 0 { "-" } else { "" },
        units.abs() / 100,
        units.abs() % 100
    )
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "bucket", DataType::UInt64, false),
            Column::new(
                3,
                "amount",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                true,
            ),
            Column::new(
                4,
                "wide",
                DataType::Decimal {
                    precision: 30,
                    scale: 2,
                },
                true,
            ),
            Column::new(5, "spelled", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

fn stored(id: u64) -> StoredRow {
    let text =
        |units: Option<i128>| units.map_or(Value::Null, |units| Value::Utf8(hundredths(units)));
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::UInt64(bucket(id)),
            text(amount(id).map(i128::from)),
            text(wide(id)),
            text(amount(id).map(i128::from)),
        ],
        1,
        false,
    )
}

/// The statement's rows and how many files it spilled, under a memory
/// ceiling of `memory` bytes.
fn run(sql: &str, memory: usize) -> Result<(Vec<Vec<String>>, u64), String> {
    let directory = tempfile::tempdir().expect("directory");
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("table");
    table
        .bulk_ingest_snapshot((1..=ROWS).map(stored).collect())
        .expect("ingest");
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TableId::new(1),
        "readings",
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
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution = Execution::start(plan, &provider, memory, Collation::default())
        .map_err(|error| error.to_string())?;
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().map_err(|error| error.to_string())? {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value_owned(row).expect("value") {
                        Value::Null => "NULL".to_owned(),
                        Value::Utf8(text) => text,
                        Value::Int64(number) => number.to_string(),
                        Value::UInt64(number) => number.to_string(),
                        Value::DecimalAverage(average) => average.label.clone(),
                        other => format!("{other:?}"),
                    })
                    .collect(),
            );
        }
    }
    Ok((rows, execution.spill_metrics().files))
}

const AMPLE: usize = 512 << 20;

#[test]
fn distinct_sums_over_units_and_over_text_answer_the_same_after_a_spill() {
    let sql = "SELECT bucket, SUM(DISTINCT amount), COUNT(DISTINCT amount), \
               SUM(DISTINCT wide), COUNT(DISTINCT wide), \
               AVG(DISTINCT amount), AVG(DISTINCT wide), SUM(DISTINCT spelled) \
               FROM readings GROUP BY bucket ORDER BY bucket";
    let (roomy, files) = run(sql, AMPLE).expect("roomy");
    assert_eq!(files, 0, "the ample ceiling holds every group");

    // The sums and counts, computed here from the generated rows.
    let mut distinct = BTreeMap::<u64, BTreeSet<i64>>::new();
    for id in 1..=ROWS {
        distinct.entry(bucket(id)).or_default().extend(amount(id));
    }
    assert_eq!(roomy.len(), distinct.len());
    let mut empty = 0;
    for (row, (bucket, amounts)) in roomy.iter().zip(&distinct) {
        let sum = |units: Vec<i128>| {
            if units.is_empty() {
                "NULL".to_owned()
            } else {
                hundredths(units.iter().sum())
            }
        };
        let narrow = amounts.iter().map(|amount| i128::from(*amount)).collect();
        let wide = amounts
            .iter()
            .map(|amount| i128::from(*amount) * WIDE + i128::from(amount.signum()) * 7)
            .collect();
        empty += usize::from(amounts.is_empty());
        assert_eq!(
            row[..5],
            [
                bucket.to_string(),
                sum(narrow),
                amounts.len().to_string(),
                sum(wide),
                amounts.len().to_string(),
            ],
            "bucket {bucket}"
        );
    }
    assert!(empty > 0, "a group whose amounts are all NULL");

    // Some ceiling in this range is too small to hold the groups and large
    // enough to run: which one depends on the width of a group's state.
    let mut spilled = 0;
    for ceiling in [3 << 20, 4 << 20, 6 << 20, 8 << 20, 12 << 20] {
        let Ok((rows, files)) = run(sql, ceiling) else {
            continue;
        };
        assert_eq!(rows, roomy, "under {ceiling} bytes, {files} spilled files");
        spilled += files;
    }
    assert!(spilled > 0, "no ceiling made the groups spill");
}
