//! `COUNT(*)` over an inner join on equalities, where each key holds many
//! rows, is counted from how many rows each key holds on either side:
//! no pair is produced.
//!
//! The expected counts come from the rows: per key, the rows that carry it
//! on the left times the rows that carry it on the right. A NULL key joins
//! nothing under `=` and joins NULL under `<=>`; text keys are one key per
//! collation class; a filter on either side removes its rows before they
//! are counted. A join on a unique key, and one with few rows per key,
//! keep the paths that produce their pairs, and answer the same.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    ExecCounters, Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    take_exec_counters,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 2_400;
const ZONES: [&str; 4] = ["north", "North", "south", "east"];

#[derive(Clone, Debug)]
struct Reading {
    id: u64,
    /// One of five instants, NULL for every eleventh row.
    slot: Option<String>,
    /// One of three zones once case is folded, NULL for every seventh row.
    zone: Option<&'static str>,
    /// Two rows share each pair number.
    pair: i64,
}

fn reading(id: u64) -> Reading {
    Reading {
        id,
        slot: (id % 11 != 0).then(|| format!("2024-05-0{} 12:00:00", 1 + id % 5)),
        zone: (id % 7 != 0).then(|| ZONES[usize::try_from(id % 4).expect("small")]),
        pair: i64::try_from(id / 2).expect("small"),
    }
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "slot", DataType::DateTime64 { fsp: 0 }, true),
            Column::new(3, "zone", DataType::Utf8, true),
            Column::new(4, "pair", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn run(sql: &str) -> (Vec<Vec<Value>>, ExecCounters) {
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
    table
        .bulk_ingest_snapshot(
            (0..ROWS)
                .map(|id| {
                    let row = reading(id);
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            row.slot.map_or(Value::Null, Value::Utf8),
                            row.zone
                                .map_or(Value::Null, |zone| Value::Utf8(zone.to_owned())),
                            Value::Int64(row.pair),
                        ],
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
        "readings",
        schema(),
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry");
    let catalog =
        CatalogSnapshot::new([DatabaseEntry::new(DatabaseId::new(1), "app", [entry])
            .expect("database")])
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
    let _ = take_exec_counters();
    let mut execution =
        Execution::start(plan, &provider, 256 << 20, Collation::default()).expect("execution");
    let mut answer = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            answer.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| column.value_owned(row).expect("value"))
                    .collect(),
            );
        }
    }
    (answer, take_exec_counters())
}

/// The pairs of the rows `left` keeps with the rows `right` keeps that
/// carry the same key; a row whose key is `None` pairs with nothing.
fn pairs<K: Ord>(
    left: impl Fn(&Reading) -> bool,
    right: impl Fn(&Reading) -> bool,
    key: impl Fn(&Reading) -> Option<K>,
) -> u64 {
    let count = |keep: &dyn Fn(&Reading) -> bool| {
        let mut counts = BTreeMap::new();
        for row in (0..ROWS).map(reading).filter(|row| keep(row)) {
            if let Some(key) = key(&row) {
                *counts.entry(key).or_insert(0_u64) += 1;
            }
        }
        counts
    };
    let right = count(&right);
    count(&left)
        .into_iter()
        .map(|(key, rows)| rows * right.get(&key).copied().unwrap_or(0))
        .sum()
}

fn all(_: &Reading) -> bool {
    true
}

fn folded(row: &Reading) -> Option<String> {
    row.zone.map(str::to_lowercase)
}

#[test]
fn many_rows_per_key_are_counted_by_key() {
    let cases: [(&str, u64); 6] = [
        (
            "SELECT COUNT(*) FROM readings a JOIN readings b ON a.slot = b.slot",
            pairs(all, all, |row| row.slot.clone()),
        ),
        (
            "SELECT COUNT(*) FROM readings a JOIN readings b ON a.zone = b.zone",
            pairs(all, all, folded),
        ),
        (
            "SELECT COUNT(*) FROM readings a JOIN readings b \
             ON a.slot = b.slot AND a.zone = b.zone",
            pairs(all, all, |row| Some((row.slot.clone()?, folded(row)?))),
        ),
        (
            "SELECT COUNT(*) FROM readings a JOIN readings b ON a.slot <=> b.slot",
            pairs(all, all, |row| Some(row.slot.clone())),
        ),
        (
            "SELECT COUNT(*) FROM readings a JOIN readings b ON a.slot = b.slot \
             WHERE a.id % 3 = 0 AND b.id > 100",
            pairs(
                |row| row.id % 3 == 0,
                |row| row.id > 100,
                |row| row.slot.clone(),
            ),
        ),
        (
            "SELECT COUNT(*), COUNT(*) + 1 FROM readings a JOIN readings b ON b.zone = a.zone",
            pairs(all, all, folded),
        ),
    ];
    for (sql, expected) in cases {
        let (answer, counters) = run(sql);
        assert_eq!(answer.len(), 1, "{sql}");
        assert_eq!(answer[0][0], Value::UInt64(expected), "{sql}");
        assert_eq!(counters.join_pairs_counted, expected, "{sql}");
    }
}

#[test]
fn few_rows_per_key_keep_the_join() {
    let cases: [(&str, u64); 3] = [
        (
            "SELECT COUNT(*) FROM readings a JOIN readings b ON a.id = b.id",
            ROWS,
        ),
        (
            "SELECT COUNT(*) FROM readings a JOIN readings b ON a.pair = b.pair",
            pairs(all, all, |row| Some(row.pair)),
        ),
        (
            "SELECT COUNT(*) FROM readings a JOIN readings b ON a.slot = b.slot \
             WHERE a.id < 0 AND b.id < 0",
            0,
        ),
    ];
    for (sql, expected) in cases {
        let (answer, counters) = run(sql);
        assert_eq!(answer, vec![vec![Value::UInt64(expected)]], "{sql}");
        assert_eq!(counters.join_pairs_counted, 0, "{sql}");
    }
}
