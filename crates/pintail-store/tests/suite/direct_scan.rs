//! A flushed segment carries one row per key, because the memtable is a map.
//! Marking it `unique_keys` lets the scan classifier take its columnar Direct
//! path instead of a row-by-row merge — but that path applies **no tombstone
//! filter**, so the flag also asserts the segment holds no deletes.
//!
//! These cases pin the boundary. Every one of them would still pass if the
//! flag were hardcoded false; what they catch is the flag being set when it
//! must not be, which resurrects deleted rows.

use pintail_store::{StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

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

fn row(id: u64, label: &str, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Utf8(label.into())],
        version,
        deleted,
    )
}

fn options() -> StoreOptions {
    StoreOptions {
        wal_sync: WalSync::Off,
        ..StoreOptions::default()
    }
}

/// Drains the streaming projected scan — the path that owns the Direct and
/// Merge classification — rather than `scan()`, which filters tombstones on
/// every branch and so cannot observe the difference.
fn projected(table: &TableStore) -> Vec<(u64, String)> {
    let snapshot = table.snapshot();
    let start = PrimaryKey::new(vec![KeyPart::UInt64(u64::MIN)]).expect("start");
    let end = PrimaryKey::new(vec![KeyPart::UInt64(u64::MAX)]).expect("end");
    let mut stream = snapshot
        .scan_projected_range_stream(&start, &end, &[1, 2])
        .expect("stream");
    let mut rows = Vec::new();
    if let Some(stream) = stream.as_mut() {
        while let Some(chunk) = stream.next_chunk(64 * 1024 * 1024).expect("chunk") {
            for values in chunk.rows() {
                rows.push(pair(values));
            }
        }
    } else {
        // The stream declines a range that needs visibility resolution below
        // its row threshold; the caller is expected to use the materialized
        // path there. Following that fallback is what makes these cases
        // compare results rather than which path served them.
        let scan = snapshot
            .scan_projected_range(&start, &end, &[1, 2])
            .expect("bounded scan");
        for projected in scan.rows() {
            rows.push(pair(projected.values()));
        }
    }
    rows.sort_unstable();
    rows
}

fn pair(values: &[Value]) -> (u64, String) {
    match (&values[0], &values[1]) {
        (Value::UInt64(id), Value::Utf8(label)) => (*id, label.clone()),
        other => panic!("unexpected projected row {other:?}"),
    }
}

#[test]
fn a_flush_carrying_a_tombstone_never_resurrects_it() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table = TableStore::open(directory.path(), schema(), options()).expect("open");
    table
        .ingest(vec![row(1, "keep", 1, false), row(2, "doomed", 1, false)])
        .expect("seed");
    table.flush().expect("flush seed");
    // The delete and a live row land in the same flush, so the segment is
    // unique-keyed but tombstone-bearing — the exact shape that must not be
    // served by the unfiltered Direct path.
    table
        .ingest(vec![row(2, "doomed", 2, true), row(3, "later", 2, false)])
        .expect("delete");
    table.flush().expect("flush delete");

    assert_eq!(
        projected(&table),
        vec![(1, "keep".to_owned()), (3, "later".to_owned())],
        "a deleted key must not reappear through the columnar path"
    );
}

#[test]
fn a_tombstone_free_flush_returns_exactly_its_rows() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table = TableStore::open(directory.path(), schema(), options()).expect("open");
    table
        .ingest((1..=200_u64).map(|id| row(id, "live", 1, false)).collect())
        .expect("seed");
    table.flush().expect("flush");

    let rows = projected(&table);
    assert_eq!(rows.len(), 200);
    assert_eq!(rows[0], (1, "live".to_owned()));
    assert_eq!(rows[199], (200, "live".to_owned()));
}

#[test]
fn overlapping_unique_segments_still_merge_to_the_newer_version() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table = TableStore::open(directory.path(), schema(), options()).expect("open");
    // Two flushes, neither with a tombstone, so both are unique-keyed — and
    // their key ranges overlap, which must still force a merge.
    table
        .ingest((1..=50_u64).map(|id| row(id, "old", 1, false)).collect())
        .expect("first");
    table.flush().expect("flush first");
    table
        .ingest((25..=75_u64).map(|id| row(id, "new", 2, false)).collect())
        .expect("second");
    table.flush().expect("flush second");

    let rows = projected(&table);
    assert_eq!(rows.len(), 75);
    assert_eq!(rows[0], (1, "old".to_owned()));
    assert_eq!(
        rows[24],
        (25, "new".to_owned()),
        "the newer version must win inside the overlap"
    );
    assert_eq!(rows[74], (75, "new".to_owned()));
}

#[test]
fn a_memtable_tombstone_suppresses_a_unique_segments_row() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table = TableStore::open(directory.path(), schema(), options()).expect("open");
    table
        .ingest(vec![row(1, "live", 1, false), row(2, "live", 1, false)])
        .expect("seed");
    table.flush().expect("flush");
    // Unflushed delete over a tombstone-free segment: the classifier must not
    // treat that range as Direct just because the segment qualifies.
    table.ingest(vec![row(2, "live", 2, true)]).expect("delete");

    assert_eq!(
        projected(&table),
        vec![(1, "live".to_owned())],
        "a memtable tombstone must suppress the segment's row"
    );
}

#[test]
fn classification_survives_close_and_reopen() {
    let directory = tempfile::tempdir().expect("tempdir");
    {
        let mut table = TableStore::open(directory.path(), schema(), options()).expect("open");
        table
            .ingest(vec![row(1, "keep", 1, false), row(2, "gone", 1, false)])
            .expect("seed");
        table.flush().expect("flush seed");
        table.ingest(vec![row(2, "gone", 2, true)]).expect("delete");
        table.flush().expect("flush delete");
        table.checkpoint().expect("checkpoint");
    }
    // The flag is persisted in the manifest, so a reader that never saw the
    // write must reach the same conclusion.
    let reopened = TableStore::open(directory.path(), schema(), options()).expect("reopen");
    assert_eq!(projected(&reopened), vec![(1, "keep".to_owned())]);
}
