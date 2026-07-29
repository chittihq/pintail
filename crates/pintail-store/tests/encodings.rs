use std::collections::BTreeSet;

use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const PLAIN: u8 = 0;
const DICTIONARY: u8 = 1;
const RLE: u8 = 2;
const BIT_PACKED: u8 = 3;
const DELTA_BIT_PACKED: u8 = 4;

#[test]
fn segment_selects_and_round_trips_every_version_one_block_encoding() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "constant", DataType::UInt64, false),
            Column::new(2, "flag", DataType::Boolean, false),
            Column::new(3, "sequence", DataType::UInt64, false),
            Column::new(4, "label", DataType::Utf8, false),
            Column::new(5, "opaque", DataType::Binary, false),
        ],
    )
    .expect("schema");
    let rows = (0..32_u64)
        .map(|id| {
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                vec![
                    Value::UInt64(7),
                    Value::Boolean(id % 2 == 0),
                    Value::UInt64(1_000 + id * 3),
                    Value::Utf8(format!("label-{}", id % 2)),
                    Value::Binary(id.to_le_bytes().to_vec()),
                ],
                id + 1,
                false,
            )
        })
        .collect::<Vec<_>>();
    let segment_path = {
        let mut table =
            TableStore::open(directory.path(), schema, StoreOptions::default()).expect("open");
        table.ingest(rows.clone()).expect("ingest");
        let path = table
            .flush()
            .expect("flush")
            .segment_path()
            .expect("segment")
            .to_path_buf();
        assert_eq!(table.snapshot().scan().expect("round trip"), rows);
        path
    };

    let encodings = block_encodings(&std::fs::read(segment_path).expect("segment bytes"));
    assert_eq!(
        encodings,
        BTreeSet::from([PLAIN, DICTIONARY, RLE, BIT_PACKED, DELTA_BIT_PACKED])
    );
}

fn block_encodings(bytes: &[u8]) -> BTreeSet<u8> {
    assert_eq!(&bytes[..5], b"PTSEG");
    let column_count = read_u32(bytes, 26) as usize;
    let mut position = 34;
    let mut encodings = BTreeSet::new();
    for _ in 0..column_count {
        position += 5;
        let block_count = take_u32(bytes, &mut position);
        for _ in 0..block_count {
            let block_length = take_u32(bytes, &mut position) as usize;
            let block = &bytes[position..position + block_length];
            let mut block_position = 4;
            skip_bytes(block, &mut block_position);
            encodings.insert(take_u8(block, &mut block_position));
            position += block_length;
            position += 8;
        }
    }
    encodings
}

fn skip_bytes(bytes: &[u8], position: &mut usize) {
    let length = take_u32(bytes, position) as usize;
    *position += length;
}

fn take_u8(bytes: &[u8], position: &mut usize) -> u8 {
    let value = bytes[*position];
    *position += 1;
    value
}

fn take_u32(bytes: &[u8], position: &mut usize) -> u32 {
    let value = read_u32(bytes, *position);
    *position += 4;
    value
}

fn read_u32(bytes: &[u8], position: usize) -> u32 {
    u32::from_le_bytes(bytes[position..position + 4].try_into().expect("u32"))
}
