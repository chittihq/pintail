//! PTSEG v2+: eligible text-carried columns store fixed-width units on the
//! wire and regenerate byte-identical canonical text on every read path.

use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(
                2,
                "amount",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                true,
            ),
            Column::new(3, "day", DataType::Date32, true),
            Column::new(4, "at", DataType::DateTime64 { fsp: 0 }, true),
        ],
    )
    .expect("schema")
}

fn key(id: u64) -> PrimaryKey {
    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key")
}

fn text_row(id: u64, amount: Option<&str>, day: Option<&str>, at: Option<&str>) -> StoredRow {
    let carrier = |value: Option<&str>| value.map_or(Value::Null, |text| Value::Utf8(text.into()));
    StoredRow::new(
        key(id),
        vec![
            Value::UInt64(id),
            carrier(amount),
            carrier(day),
            carrier(at),
        ],
        id,
        false,
    )
}

/// Walks a segment's column directory and returns each column's
/// `(id, wire type tag)`.
fn wire_types(bytes: &[u8]) -> Vec<(u32, u8)> {
    let u32_at = |offset: usize| {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32 bytes"))
    };
    assert_eq!(&bytes[..5], b"PTSEG");
    let column_count = u32_at(26) as usize;
    // header: magic(5) version(1) schema-version(4) fingerprint(8)
    // row-count(8) column-count(4) block-rows(4)
    let mut offset = 34;
    let mut columns = Vec::with_capacity(column_count);
    for _ in 0..column_count {
        let id = u32_at(offset);
        let tag = bytes[offset + 4];
        let block_count = u32_at(offset + 5) as usize;
        offset += 9;
        for _ in 0..block_count {
            let payload = u32_at(offset) as usize;
            offset += 4 + payload + 8;
        }
        columns.push((id, tag));
    }
    columns
}

#[test]
fn eligible_columns_store_units_and_regenerate_identical_text() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let rows = vec![
        text_row(
            1,
            Some("123.45"),
            Some("2024-02-29"),
            Some("2023-06-15 12:34:56"),
        ),
        text_row(2, Some("-0.05"), None, Some("1970-01-01 00:00:00")),
        text_row(3, None, Some("0000-03-01"), None),
        text_row(
            4,
            Some("9999999999.99"),
            Some("9999-12-31"),
            Some("9999-12-31 23:59:59"),
        ),
    ];
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    table.ingest(rows.clone()).expect("ingest");
    let flush = table.flush().expect("flush");
    let segment_path = flush.segment_path().expect("segment path").to_path_buf();

    let bytes = std::fs::read(&segment_path).expect("segment bytes");
    for (id, tag) in wire_types(&bytes) {
        // amount, day, at: stored as Int64 units (tag 1), not text.
        if let 2..=4 = id {
            assert_eq!(tag, 1, "column {id} stores native units");
        }
    }

    // Segment-only read path.
    assert_eq!(
        table.snapshot().scan().expect("segment scan"),
        rows,
        "units regenerate byte-identical text"
    );

    // Merge path: a memtable overlay forces the streaming k-way merge over
    // the native segment.
    let extra = text_row(5, Some("0.01"), Some("1970-01-01"), None);
    table.ingest(vec![extra.clone()]).expect("overlay ingest");
    let mut merged = rows.clone();
    merged.push(extra);
    assert_eq!(
        table.snapshot().scan().expect("merged scan"),
        merged,
        "merge path regenerates identical text"
    );

    // Projected path (late materialization).
    let projected = table
        .snapshot()
        .scan_projected_range_bounded(&key(1), &key(4), &[2, 3, 4], 64 * 1024 * 1024)
        .expect("projected scan");
    let scanned = projected
        .rows()
        .iter()
        .map(|row| row.values().to_vec())
        .collect::<Vec<_>>();
    let expected = rows
        .iter()
        .map(|row| row.values()[1..].to_vec())
        .collect::<Vec<_>>();
    assert_eq!(
        scanned, expected,
        "projected reads regenerate identical text"
    );
}

#[test]
fn non_canonical_text_keeps_the_column_on_the_text_path() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    // "01.50" parses but formats back as "1.50": not canonical, so the
    // whole column must stay text and round-trip the original bytes.
    let rows = vec![
        text_row(1, Some("123.45"), None, None),
        text_row(2, Some("01.50"), None, None),
    ];
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    table.ingest(rows.clone()).expect("ingest");
    let flush = table.flush().expect("flush");
    let bytes = std::fs::read(flush.segment_path().expect("segment path")).expect("segment bytes");
    for (id, tag) in wire_types(&bytes) {
        if id == 2 {
            assert_eq!(tag, 4, "non-canonical decimal column stays Utf8");
        }
    }

    assert_eq!(
        table.snapshot().scan().expect("scan"),
        rows,
        "original non-canonical text survives"
    );
}
