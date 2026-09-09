//! Sparse, nullable, low-compressibility baseline/candidate correctness probe.
#![allow(clippy::too_many_lines)]
use pintail_store::{DecodedColumn, StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
use std::{io::Write, path::Path};

fn key(id: u64) -> PrimaryKey {
    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key")
}
fn predicate(id: u64) -> Value {
    let block = id / 16_384;
    if block % 32 == 7 || id.is_multiple_of(97) {
        Value::Null
    } else if block % 32 == 3 || (131_070..131_075).contains(&(id % 524_288)) {
        Value::Utf8("match".into())
    } else {
        Value::Utf8("other".into())
    }
}
fn values(id: u64) -> Vec<Value> {
    let mut state = id.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut payload = Vec::with_capacity(64);
    for _ in 0..8 {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        payload.extend_from_slice(&state.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
    }
    vec![
        Value::UInt64(id),
        predicate(id),
        if id.is_multiple_of(13) {
            Value::Null
        } else {
            Value::Binary(payload)
        },
        if id.is_multiple_of(17) {
            Value::Null
        } else {
            Value::Utf8(format!(
                "variable-{id}-{}",
                "x".repeat(usize::try_from(id % 19).expect("length"))
            ))
        },
    ]
}
fn matches(value: &Value, mode: usize) -> bool {
    match mode {
        0 | 1 => *value == Value::Utf8("match".into()),
        2 => *value == Value::Null,
        _ => *value == Value::Utf8("absent".into()),
    }
}
fn run(directory: &Path, rows: u64, output: &Path) {
    assert!(rows > 0);
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "row_id", DataType::UInt64, false),
            Column::new(2, "selector", DataType::Utf8, true),
            Column::new(3, "opaque", DataType::Binary, true),
            Column::new(4, "description", DataType::Utf8, true),
        ],
    )
    .expect("schema");
    let mut table = TableStore::open(
        directory,
        schema,
        StoreOptions {
            wal_sync: WalSync::Off,
            background_compaction: false,
            ..StoreOptions::default()
        },
    )
    .expect("open");
    if table.snapshot().key_bounds().is_none() {
        for start in (0..rows).step_by(524_288) {
            table
                .bulk_ingest_snapshot(
                    (start..(start + 524_288).min(rows))
                        .map(|id| StoredRow::new(key(id), values(id), 0, false))
                        .collect(),
                )
                .expect("seed");
        }
    }
    let snapshot = table.snapshot();
    assert_eq!(snapshot.physical_row_upper_bound(), rows);
    std::fs::create_dir_all(output).expect("output");
    for mode in 0..4 {
        let projection = if mode == 0 { vec![2] } else { vec![4, 1, 3, 2] };
        let mut expected = (0..rows).filter(|id| matches(&predicate(*id), mode));
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(output.join(format!("case-{mode}.txt"))).expect("file"),
        );
        let mut stream = snapshot
            .scan_projected_range_stream(&key(0), &key(rows - 1), &projection)
            .expect("scan")
            .expect("stream");
        let select = |columns: &[DecodedColumn],
                      count: usize|
         -> Result<Option<Vec<std::ops::Range<usize>>>, String> {
            let mut ranges = Vec::new();
            let mut start = None;
            for row in 0..=count {
                let keep = row < count && matches(&columns[0].value_at(row).expect("value"), mode);
                if keep {
                    start.get_or_insert(row);
                } else if let Some(begin) = start.take() {
                    ranges.push(begin..row);
                }
            }
            Ok(Some(ranges))
        };
        let (mut count, mut decoded, mut pruned) = (0, 0, 0);
        loop {
            let chunks = stream
                .next_column_chunks_filtered(1, 256 * 1024 * 1024, &[2], &select)
                .expect("chunks");
            if chunks.is_empty() {
                break;
            }
            for chunk in chunks {
                decoded += chunk.stats().blocks_decoded();
                pruned += chunk.stats().blocks_pruned();
                for row in 0..chunk.row_count() {
                    let id = expected.next().expect("unexpected extra row");
                    let expected_values = values(id);
                    let actual: Vec<_> = chunk
                        .columns()
                        .iter()
                        .map(|column| column.value_at(row).expect("value"))
                        .collect();
                    let wanted: Vec<_> = projection
                        .iter()
                        .map(|column| {
                            expected_values[usize::try_from(*column - 1).expect("column")].clone()
                        })
                        .collect();
                    assert_eq!(actual, wanted, "case {mode}, row {id}");
                    writeln!(file, "{actual:?}").expect("write");
                    count += 1;
                }
            }
        }
        assert!(expected.next().is_none(), "dropped row, case {mode}");
        file.flush().expect("flush");
        println!(
            "case={mode} fixture_rows={rows} selected={count} decoded={decoded} pruned={pruned}"
        );
    }
}
#[allow(dead_code)] // Also compiled into the integration-test harness.
fn main() {
    let args: Vec<_> = std::env::args().collect();
    run(
        Path::new(&args[1]),
        args[2].parse().expect("rows"),
        Path::new(&args[3]),
    );
}

#[test]
fn sparse_nullable_blocks_preserve_rows() {
    let data = tempfile::tempdir().expect("data");
    let output = tempfile::tempdir().expect("output");
    run(data.path(), 262_161, output.path());
}
