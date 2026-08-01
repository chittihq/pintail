//! GOAL.md §5.1 no-op suppression: repeated polling scans of unchanged data
//! must leave storage byte-identical, while real changes still land.

use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

fn key(id: u64) -> PrimaryKey {
    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key")
}

fn row(id: u64, label: &str, version: u64) -> StoredRow {
    StoredRow::new(
        key(id),
        vec![Value::UInt64(id), Value::Utf8(label.into())],
        version,
        false,
    )
}

fn segment_bytes(directory: &std::path::Path) -> Vec<(String, u64)> {
    let mut files = std::fs::read_dir(directory)
        .expect("table directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".ptseg"))
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.metadata().expect("segment metadata").len(),
            )
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

#[test]
fn idle_scan_cycles_leave_storage_byte_identical() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    let baseline_rows = (1..=500_u64)
        .map(|id| row(id, &format!("label-{id}"), 1))
        .collect::<Vec<_>>();
    table
        .ingest(baseline_rows.clone())
        .expect("baseline ingest");
    table.flush().expect("baseline flush");
    let baseline_files = segment_bytes(directory.path());
    let baseline_metrics = table.metrics().expect("baseline metrics");
    assert_eq!(baseline_metrics.segment_count(), 1);

    // Ten idle sync cycles: every scan re-reads the same unchanged rows
    // (as polling does each cycle), at ever-higher scan versions.
    for cycle in 0..10_u64 {
        let rescan = (1..=500_u64)
            .map(|id| row(id, &format!("label-{id}"), 2 + cycle))
            .collect::<Vec<_>>();
        let outcome = table.ingest_scan(rescan).expect("idle scan cycle");
        assert_eq!(outcome.accepted_rows(), 0, "cycle {cycle} accepted rows");
        table.flush().expect("idle flush");
    }
    assert_eq!(
        segment_bytes(directory.path()),
        baseline_files,
        "ten idle cycles must not change stored bytes"
    );
    let idle_metrics = table.metrics().expect("idle metrics");
    assert_eq!(idle_metrics.segment_count(), 1);
    assert_eq!(idle_metrics.memtable_bytes(), 0);

    // A genuine change in the next scan still lands.
    let mut changed = (1..=500_u64)
        .map(|id| row(id, &format!("label-{id}"), 20))
        .collect::<Vec<_>>();
    changed[41] = row(42, "label-42-changed", 20);
    let outcome = table.ingest_scan(changed).expect("changed scan");
    assert_eq!(
        outcome.accepted_rows(),
        1,
        "only the changed row is ingested"
    );
    let visible = table
        .snapshot()
        .scan()
        .expect("scan after change")
        .into_iter()
        .find(|stored| stored.key() == &key(42))
        .expect("row 42");
    assert_eq!(visible.values()[1], Value::Utf8("label-42-changed".into()));
}

#[test]
fn deleted_and_reinserted_scan_rows_are_never_suppressed() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    table
        .ingest(vec![row(1, "alpha", 1), row(2, "beta", 1)])
        .expect("baseline");
    table.flush().expect("flush");

    // The reconciler tombstones row 1.
    let tombstone = StoredRow::new(key(1), vec![Value::UInt64(1), Value::Null], 2, true);
    table.ingest(vec![tombstone.clone()]).expect("tombstone");

    // Re-scanning the tombstoned key with the OLD content must not be
    // suppressed against the pre-delete version.
    let outcome = table
        .ingest_scan(vec![row(1, "alpha", 3)])
        .expect("reinsert scan");
    assert_eq!(outcome.accepted_rows(), 1, "reinsert after delete lands");

    // Delete again above the reinsert, then re-scan the tombstone: an
    // identical tombstone IS a no-op.
    table
        .ingest(vec![StoredRow::new(
            key(1),
            vec![Value::UInt64(1), Value::Null],
            4,
            true,
        )])
        .expect("re-delete");
    table.flush().expect("flush tombstone");
    let outcome = table
        .ingest_scan(vec![StoredRow::new(
            key(1),
            vec![Value::UInt64(1), Value::Null],
            9,
            true,
        )])
        .expect("idempotent tombstone scan");
    assert_eq!(outcome.accepted_rows(), 0, "identical tombstone suppressed");
}
