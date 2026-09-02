//! A segment larger than a scan's memory budget is read in row slices, not
//! refused. A compacted table holds tens of millions of rows in one segment,
//! and a reconciliation streaming it under a fixed budget found the stream
//! answering "projected scan memory limit exceeded" for the whole segment.

use pintail_store::{StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "payload", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

#[test]
fn a_segment_beyond_the_budget_streams_in_slices_with_every_row() {
    const ROWS: u64 = 60_000;
    let directory = tempfile::tempdir().expect("store directory");
    let mut writer = TableStore::open(
        directory.path(),
        schema(),
        StoreOptions {
            wal_sync: WalSync::Off,
            ..StoreOptions::default()
        },
    )
    .expect("writer");
    let rows = (0..ROWS)
        .map(|id| {
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                vec![Value::UInt64(id), Value::Utf8(format!("{id:0>48}"))],
                1,
                false,
            )
        })
        .collect::<Vec<_>>();
    writer.ingest(rows).expect("ingest");
    writer.flush().expect("flush into one segment");

    let snapshot = writer.snapshot();
    let (first, last) = snapshot.key_bounds().expect("bounds");
    // Below the segment's decoded size, above one block's: the reader
    // charges a whole 16,384-row block per decode, so a slice can be no
    // finer than a block, and sixty thousand 48-byte payloads span four.
    let budget = 2 * 1024 * 1024;
    let mut stream = snapshot
        .scan_projected_range_stream(&first, &last, &[1, 2])
        .expect("stream")
        .expect("a flushed segment streams");
    let mut seen = 0_u64;
    let mut chunks = 0;
    let mut previous: Option<u64> = None;
    while let Some(chunk) = stream
        .next_chunk(budget)
        .expect("a slice within the budget")
    {
        chunks += 1;
        for values in chunk.into_rows() {
            let Value::UInt64(id) = values[0] else {
                panic!("id column");
            };
            assert!(
                previous.is_none_or(|last| last < id),
                "rows arrive in key order"
            );
            assert_eq!(values[1], Value::Utf8(format!("{id:0>48}")));
            previous = Some(id);
            seen += 1;
        }
    }
    assert_eq!(seen, ROWS, "every row of the segment arrives");
    assert!(chunks > 1, "the segment was sliced ({chunks} chunks)");
}
