//! Pins the sweep-line scan partitioning: disjoint unique segments decode
//! directly (whole-segment chunks) while only the overlapping cluster pays the
//! bounded merge, with memtable rows served in their gap and inside the
//! cluster, matching the naive full-scan reference exactly.

use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn key(value: u64) -> PrimaryKey {
    PrimaryKey::new(vec![KeyPart::UInt64(value)]).expect("key")
}

fn row(id: u64, text: String, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(key(id), vec![Value::Utf8(text)], version, deleted)
}

#[test]
fn partitioned_scan_merges_only_the_overlapping_cluster_and_matches_reference() {
    let directory = tempfile::tempdir().expect("table directory");
    let schema =
        TableSchema::new(1, vec![Column::new(1, "value", DataType::Utf8, false)]).expect("schema");
    let mut writer =
        TableStore::open(directory.path(), schema, StoreOptions::default()).expect("writer");

    // Segment A: keys 1..=70_000, disjoint from everything else.
    writer
        .bulk_ingest_snapshot(
            (1..=70_000)
                .map(|id| row(id, format!("a-{id}"), 0, false))
                .collect(),
        )
        .expect("segment A");
    // Segment B: keys 100_001..=170_000.
    writer
        .bulk_ingest_snapshot(
            (100_001..=170_000)
                .map(|id| row(id, format!("b-{id}"), 0, false))
                .collect(),
        )
        .expect("segment B");
    // Segment C: keys 150_000..=170_000, newer versions overlapping B.
    writer
        .bulk_ingest_snapshot(
            (150_000..=170_000)
                .map(|id| row(id, format!("c-{id}"), 1, false))
                .collect(),
        )
        .expect("segment C");
    // WAL rows: a run in the A..B gap, one update inside the B/C cluster, one
    // tombstone inside the cluster, one tombstone for a key that never existed.
    let mut wal = (80_000..80_010)
        .map(|id| row(id, format!("wal-{id}"), 2, false))
        .collect::<Vec<_>>();
    wal.push(row(160_000, "wal-160000".into(), 5, false));
    wal.push(row(150_123, String::new(), 9, true));
    wal.push(row(80_500, String::new(), 2, true));
    writer.ingest(wal).expect("WAL rows");

    let snapshot = writer.snapshot();
    let reference: Vec<Value> = snapshot
        .scan()
        .expect("reference scan")
        .into_iter()
        .map(|stored| stored.values()[0].clone())
        .collect();
    assert_eq!(reference.len(), 70_000 + 70_000 + 10 - 1);
    let (start, end) = snapshot.key_bounds().expect("bounds");
    let mut stream = snapshot
        .scan_projected_range_stream(&start, &end, &[1])
        .expect("open stream")
        .expect("partitioned stream is available despite overlap and WAL rows");
    assert_eq!(stream.segment_count(), 3);

    let mut streamed = Vec::new();
    let mut chunk_rows = Vec::new();
    let mut segments_read = 0;
    while let Some(chunk) = stream.next_chunk(64 * 1024 * 1024).expect("stream chunk") {
        chunk_rows.push(chunk.rows().len());
        segments_read += chunk.stats().segments_read();
        for values in chunk.rows() {
            streamed.push(values[0].clone());
        }
    }

    // Segment A must have arrived as one direct whole-segment chunk — the old
    // all-or-nothing classifier forced every row through <=8192-row merges.
    assert!(
        chunk_rows.contains(&70_000),
        "expected a direct whole-segment chunk for segment A, got {chunk_rows:?}"
    );
    assert_eq!(segments_read, 3);
    assert_eq!(streamed.len(), reference.len());
    assert_eq!(streamed, reference);

    // Spot-check semantics across part kinds.
    assert!(streamed.contains(&Value::Utf8("wal-80004".into())));
    assert!(streamed.contains(&Value::Utf8("wal-160000".into())));
    assert!(streamed.contains(&Value::Utf8("c-150000".into())));
    assert!(!streamed.contains(&Value::Utf8("b-150123".into())));
    assert!(!streamed.contains(&Value::Utf8("c-150123".into())));
}

#[test]
fn granule_refinement_splits_base_and_tail_clusters() {
    let directory = tempfile::tempdir().expect("table directory");
    let schema =
        TableSchema::new(1, vec![Column::new(1, "value", DataType::Utf8, false)]).expect("schema");
    let mut writer =
        TableStore::open(directory.path(), schema, StoreOptions::default()).expect("writer");

    // Base: 130_000 unique keys; tail: 8_000 newer versions clustered at the
    // top of the keyspace (the CDC shape). Dominance 130k >= 4 * 8k holds.
    writer
        .bulk_ingest_snapshot(
            (1..=130_000)
                .map(|id| row(id, format!("base-{id}"), 0, false))
                .collect(),
        )
        .expect("base segment");
    writer
        .bulk_ingest_snapshot(
            (122_001..=130_000)
                .map(|id| row(id, format!("tail-{id}"), 1, false))
                .collect(),
        )
        .expect("tail segment");

    let snapshot = writer.snapshot();
    let reference: Vec<Value> = snapshot
        .scan()
        .expect("reference scan")
        .into_iter()
        .map(|stored| stored.values()[0].clone())
        .collect();
    assert_eq!(reference.len(), 130_000);
    let (start, end) = snapshot.key_bounds().expect("bounds");
    let mut stream = snapshot
        .scan_projected_range_stream(&start, &end, &[1])
        .expect("open stream")
        .expect("stream available");

    let mut streamed = Vec::new();
    let mut chunk_rows = Vec::new();
    while let Some(chunk) = stream.next_chunk(64 * 1024 * 1024).expect("stream chunk") {
        chunk_rows.push(chunk.rows().len());
        for values in chunk.rows() {
            streamed.push(values[0].clone());
        }
    }
    assert_eq!(streamed.len(), reference.len());
    assert_eq!(streamed, reference);
    // The refined plan serves the untouched prefix of the base as one large
    // direct row-range chunk instead of pushing all 138k rows through the
    // merge (which caps chunks at 8k rows).
    assert!(
        chunk_rows.iter().any(|rows| *rows >= 64 * 1024),
        "expected a large direct-range chunk from granule refinement, got {chunk_rows:?}"
    );
    assert!(streamed.contains(&Value::Utf8("tail-125000".into())));
    assert!(streamed.contains(&Value::Utf8("base-122000".into())));
    assert!(!streamed.contains(&Value::Utf8("base-125000".into())));
}
