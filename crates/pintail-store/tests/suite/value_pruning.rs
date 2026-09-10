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
    // touches nothing else, so its statistics alone decide it. The middle
    // segment overlaps the last, but only a NEWER one: every row it holds is
    // current and fails the bound, or stale behind the last segment's
    // version, so it prunes too.
    let (rows, pruned) = scan(&table, &bucket_equals(3));
    assert_eq!(pruned, 2, "both non-matching segments older than their overlaps prune");
    assert!(
        rows.iter().all(|row| row[0] != Value::UInt64(1)),
        "pruned segment's keys must not appear"
    );
    assert_eq!(rows.len(), 101, "the last segment's keys 250..=350 alone");

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

/// The shape a replicated table takes: a base loaded in key-disjoint
/// chunks, then a newer segment of updates scattered across the whole key
/// range. Every base chunk overlaps that newer segment, which used to
/// switch value pruning off for all of them. A base chunk hides no newer
/// version - what it holds is current and fails the bound, or stale behind
/// the updates - so each chunk the bound excludes still prunes, while the
/// updates, which overlap older rows, are always read.
#[test]
fn a_base_overlapped_only_by_newer_updates_still_prunes() {
    let directory = tempfile::tempdir().expect("tempdir");
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("open");
    for chunk in 0..4_u64 {
        let bucket = i64::try_from(chunk * 10).expect("small");
        let keys = chunk * 100 + 1..=chunk * 100 + 100;
        table
            .ingest(keys.map(|id| row(id, bucket, 1)).collect())
            .expect("ingest");
        table.flush().expect("flush");
    }
    // Every tenth key, across all four chunks, moves to bucket 99.
    table
        .ingest((1..=400).step_by(10).map(|id| row(id, 99, 2)).collect())
        .expect("ingest");
    table.flush().expect("flush");

    // Bucket 20 is the third chunk's: the other three prune.
    let (rows, pruned) = scan(&table, &bucket_equals(20));
    assert_eq!(pruned, 3, "every base chunk the bound excludes prunes");
    let matching = rows
        .iter()
        .filter(|row| row[1] == Value::Int64(20))
        .count();
    assert_eq!(matching, 90, "the chunk's rows less the ten updated away");
    assert!(
        rows.iter()
            .all(|row| row[1] == Value::Int64(20) || row[1] == Value::Int64(99)),
        "no stale version of an updated key comes back"
    );

    // Bucket 99 lives only in the updates: all four chunks prune, and the
    // updated keys come back with their new bucket.
    let (rows, pruned) = scan(&table, &bucket_equals(99));
    assert_eq!(pruned, 4);
    assert_eq!(rows.len(), 40);
    assert!(rows.iter().all(|row| row[1] == Value::Int64(99)));
}
