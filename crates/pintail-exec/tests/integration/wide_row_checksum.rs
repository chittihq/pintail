//! A checksum over every column of a wide table, under a modest ceiling.
//!
//! `SUM(CRC32(CONCAT_WS('#', c1, ..., c20)))` projects its argument a batch
//! at a time, one row after another. What a row's evaluation allocates - a
//! value and a string per column, the joined text - is gone before the next
//! row's begins, and the result is one integer. The projection asked for
//! every row's working memory added together instead: several kilobytes a
//! row, so a batch of a few tens of thousands of rows asked for more than
//! the whole ceiling and the statement failed over a table that fits in a
//! tenth of it.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 40_000;
const CEILING: usize = 48 << 20;
const TEXT_COLUMNS: u32 = 12;

fn schema() -> TableSchema {
    let mut columns = vec![
        Column::new(1, "id", DataType::UInt64, false),
        Column::new(2, "units", DataType::Int64, false),
        Column::new(3, "total", DataType::Int64, true),
        Column::new(
            4,
            "price",
            DataType::Decimal {
                precision: 12,
                scale: 2,
            },
            true,
        ),
        Column::new(5, "stamp", DataType::DateTime64 { fsp: 0 }, false),
    ];
    for index in 0..TEXT_COLUMNS {
        columns.push(Column::new(
            6 + index,
            format!("note_{index}"),
            DataType::Utf8,
            true,
        ));
    }
    TableSchema::new(1, columns).expect("schema")
}

/// The row's values, and the text `CONCAT_WS('#', ...)` joins them into:
/// every column that is not NULL, in order.
fn row(id: u64) -> (Vec<Value>, String) {
    let mut values = vec![
        Value::UInt64(id),
        Value::Int64(i64::try_from(id % 977).expect("small") - 400),
    ];
    let mut joined = vec![id.to_string(), (i128::from(id % 977) - 400).to_string()];
    if id.is_multiple_of(7) {
        values.push(Value::Null);
    } else {
        let total = i64::try_from(id * 1_000_003).expect("fits");
        values.push(Value::Int64(total));
        joined.push(total.to_string());
    }
    if id.is_multiple_of(5) {
        values.push(Value::Null);
    } else {
        let price = format!("{}.{:02}", id % 99_991, id % 100);
        values.push(Value::Utf8(price.clone()));
        joined.push(price);
    }
    let stamp = format!(
        "2026-{:02}-{:02} {:02}:{:02}:{:02}",
        1 + id % 12,
        1 + id % 28,
        id % 24,
        id % 60,
        id * 7 % 60
    );
    values.push(Value::Utf8(stamp.clone()));
    joined.push(stamp);
    for index in 0..u64::from(TEXT_COLUMNS) {
        if (id + index).is_multiple_of(6) {
            values.push(Value::Null);
            continue;
        }
        let repeats = 1 + usize::try_from((id + index) % 5).expect("small");
        let text = format!("{index}-{id}-").repeat(repeats);
        values.push(Value::Utf8(text.clone()));
        joined.push(text);
    }
    (values, joined.join("#"))
}

/// The checksum `CRC32` answers: the reflected polynomial, a bit at a time.
fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0_u32, |crc, byte| {
        (0..8).fold(crc ^ u32::from(*byte), |crc, _| {
            (crc >> 1) ^ (0xEDB8_8320 & 0_u32.wrapping_sub(crc & 1))
        })
    })
}

#[test]
fn a_checksum_over_every_column_fits_a_modest_ceiling() {
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
    let mut sum = 0_u64;
    let mut xor = 0_u64;
    let mut rows = Vec::new();
    for id in 1..=ROWS {
        let (values, joined) = row(id);
        let checksum = u64::from(crc32(joined.as_bytes()));
        sum += checksum;
        xor ^= checksum;
        rows.push(StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
            values,
            id,
            false,
        ));
    }
    // The newest rows stay in the memtable, as a replicated table's do.
    let recent = rows.split_off(rows.len() - 3_000);
    table.bulk_ingest_snapshot(rows).expect("copied rows");
    table.ingest_cdc(recent).expect("replicated rows");

    let entry = TableEntry::new(
        TableId::new(1),
        "ledger",
        schema(),
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let columns = schema()
        .columns()
        .iter()
        .map(|column| column.name().to_owned())
        .collect::<Vec<_>>()
        .join(", ");

    for (aggregate, expected) in [("SUM", sum), ("BIT_XOR", xor)] {
        let sql =
            format!("SELECT COUNT(*), {aggregate}(CRC32(CONCAT_WS('#', {columns}))) FROM ledger");
        let snapshot = table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&parse_statement(&sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(physical, &provider, CEILING, Collation::default()).expect("start");
        let mut answer = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{aggregate} under {} MiB: {error}", CEILING >> 20))
        {
            for row in batch.selection().selected_rows() {
                answer.push(
                    (0..batch.columns().len())
                        .map(|column| {
                            match batch.column(column).and_then(|column| column.value(row)) {
                                Some(Value::UInt64(number)) => number.to_string(),
                                Some(Value::Int64(number)) => number.to_string(),
                                Some(Value::Utf8(text)) => text.clone(),
                                other => panic!("unexpected value {other:?}"),
                            }
                        })
                        .collect::<Vec<_>>(),
                );
            }
        }
        assert_eq!(
            answer,
            vec![vec![ROWS.to_string(), expected.to_string()]],
            "{aggregate}"
        );
        assert!(
            execution.memory().peak() <= CEILING,
            "{aggregate}: peak {}",
            execution.memory().peak()
        );
    }
}
