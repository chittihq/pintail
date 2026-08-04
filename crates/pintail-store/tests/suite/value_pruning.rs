use pintail_store::{BoundDomain, ColumnBounds, StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "bucket", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64, bucket: i64, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Int64(bucket)],
        version,
        false,
    )
}

fn bucket_equals(value: i128) -> Vec<ColumnBounds> {
    vec![ColumnBounds {
        column_id: 2,
        domain: BoundDomain::Int,
        lower: Some(value),
        upper: Some(value),
    }]
}

/// Segment `A` covers keys 1..=100 and touches nothing else. Segments `B`
/// and `C` share keys 250..=300, where `C` holds the winning versions.
fn overlapping_table(directory: &std::path::Path) -> TableStore {
    let mut table = TableStore::open(directory, schema(), StoreOptions::default()).expect("open");
    for (keys, bucket, version) in [
        (1..=100_u64, 1_i64, 1_u64),
        (200..=300, 2, 2),
        (250..=350, 3, 3),
    ] {
        let batch = keys.map(|id| row(id, bucket, version)).collect();
        table.ingest(batch).expect("ingest");
        table.flush().expect("flush");
    }
    table
}

fn scan(table: &TableStore, bounds: &[ColumnBounds]) -> (Vec<Vec<Value>>, usize) {
    let snapshot = table.snapshot();
    let start = PrimaryKey::new(vec![KeyPart::UInt64(u64::MIN)]).expect("start");
    let end = PrimaryKey::new(vec![KeyPart::UInt64(u64::MAX)]).expect("end");
    let scan = snapshot
        .scan_projected_range_bounded_pruned(&start, &end, &[1, 2], 64 * 1024 * 1024, bounds)
        .expect("pruned scan");
    let pruned = scan.stats().segments_pruned();
    let rows = scan
        .into_rows()
        .into_iter()
        .map(pintail_store::ProjectedRow::into_values)
        .collect();
    (rows, pruned)
}

#[test]
fn an_isolated_segment_prunes_while_overlapping_neighbours_are_read() {
    let directory = tempfile::tempdir().expect("tempdir");
    let table = overlapping_table(directory.path());

    // Bucket 3 lives only in the last segment. The first segment's key range
    // touches nothing else, so its statistics alone decide it, even though
    // two other segments in the same manifest overlap each other.
    let (rows, pruned) = scan(&table, &bucket_equals(3));
    assert_eq!(pruned, 1, "the isolated non-matching segment must prune");
    assert!(
        rows.iter().all(|row| row[0] != Value::UInt64(1)),
        "pruned segment's keys must not appear"
    );

    // Whole-manifest pruning would have refused here: the manifest has both
    // an overlapping pair and, after the merge, tombstone-free statistics
    // are not enough on their own.
    let (all_rows, none_pruned) = scan(&table, &[]);
    assert_eq!(none_pruned, 0);
    assert_eq!(all_rows.len(), 251, "keys 1..=100 and 200..=350 survive");
}

#[test]
fn overlapping_segments_never_prune_away_a_winning_version() {
    let directory = tempfile::tempdir().expect("tempdir");
    let table = overlapping_table(directory.path());

    // Bucket 2 selects the middle segment. Its overlapping neighbour holds
    // newer versions of keys 250..=300 that do not match the bound: pruning
    // that neighbour would resurrect the stale bucket-2 rows underneath it.
    let (rows, _) = scan(&table, &bucket_equals(2));
    let bucket_of = |target: u64| {
        rows.iter()
            .find(|row| row[0] == Value::UInt64(target))
            .map(|row| row[1].clone())
    };
    assert_eq!(bucket_of(200), Some(Value::Int64(2)));
    assert_eq!(
        bucket_of(275),
        Some(Value::Int64(3)),
        "the newer version must still win over the bound-matching one"
    );
    assert_eq!(bucket_of(340), Some(Value::Int64(3)));
    assert_eq!(rows.len(), 151, "only the bucket-1 segment prunes away");
}
