//! The overlay and the layered scan over keys that are not one integer: a
//! text key, a binary key, and composite keys mixing text with integers.
//! Each table takes updates, deletes and inserts between its stored keys,
//! in the memtable and flushed over its bases, and every state is read with
//! the key's columns named (the overlay) and row by row; both must show the
//! model's rows in key order.

use super::*;
use pintail_types::{Column, DataType, Value};
use std::collections::BTreeMap;

/// How a table of these tests is keyed.
#[derive(Clone, Copy, Debug)]
enum Shape {
    Text,
    Binary,
    IntegerThenText,
    TextThenInteger,
}

impl Shape {
    const ALL: [Self; 4] = [
        Self::Text,
        Self::Binary,
        Self::IntegerThenText,
        Self::TextThenInteger,
    ];

    fn key_columns(self) -> Vec<Column> {
        match self {
            Self::Text => vec![Column::new(1, "code", DataType::Utf8, false)],
            Self::Binary => vec![Column::new(1, "code", DataType::Binary, false)],
            Self::IntegerThenText => vec![
                Column::new(1, "shelf", DataType::Int64, false),
                Column::new(2, "code", DataType::Utf8, false),
            ],
            Self::TextThenInteger => vec![
                Column::new(1, "code", DataType::Utf8, false),
                Column::new(2, "slot", DataType::UInt64, false),
            ],
        }
    }

    fn key_ids(self) -> Vec<u32> {
        self.key_columns().iter().map(Column::id).collect()
    }

    /// Text that orders unlike the number it is made from, of several
    /// lengths, with upper and lower case, a trailing space, a prefix of
    /// another key and bytes past ASCII among them.
    fn text(id: u64) -> String {
        let scrambled = id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 20;
        match id % 6 {
            0 => format!("{scrambled:011x}"),
            1 => format!("K{scrambled:x}"),
            2 => format!("k{scrambled:x} "),
            3 => format!("\u{e9}t\u{e9}-{scrambled:x}"),
            4 => format!(
                "{:011x}-tail",
                (id - 4).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 20
            ),
            _ => format!("{scrambled:x}\u{4e2d}"),
        }
    }

    fn key(self, id: u64) -> PrimaryKey {
        let parts = match self {
            Self::Text => vec![KeyPart::Utf8(Self::text(id))],
            Self::Binary => {
                let mut bytes = Self::text(id).into_bytes();
                bytes.push(u8::try_from(id % 256).unwrap());
                bytes.insert(0, u8::try_from(id.wrapping_mul(131) % 256).unwrap());
                vec![KeyPart::Binary(bytes)]
            }
            Self::IntegerThenText => vec![
                KeyPart::Int64(i64::try_from(id % 7).unwrap() - 3),
                KeyPart::Utf8(Self::text(id)),
            ],
            Self::TextThenInteger => vec![
                KeyPart::Utf8(Self::text(id / 3 * 6)),
                KeyPart::UInt64(id % 3),
            ],
        };
        PrimaryKey::new(parts).unwrap()
    }

    fn schema(self) -> TableSchema {
        let mut columns = self.key_columns();
        let next = u32::try_from(columns.len()).unwrap() + 1;
        columns.push(Column::new(next, "amount", DataType::Int64, true));
        columns.push(Column::new(next + 1, "label", DataType::Utf8, false));
        TableSchema::new(1, columns).unwrap()
    }
}

fn key_values(key: &PrimaryKey) -> Vec<Value> {
    key.parts()
        .iter()
        .map(|part| match part {
            KeyPart::Int64(value) => Value::Int64(*value),
            KeyPart::UInt64(value) => Value::UInt64(*value),
            KeyPart::Utf8(value) => Value::Utf8(value.clone()),
            KeyPart::Binary(value) => Value::Binary(value.clone()),
        })
        .collect()
}

fn label(amount: Option<i64>) -> String {
    format!("label {}", amount.unwrap_or(-1) % 9)
}

fn row(key: &PrimaryKey, amount: Option<i64>, version: u64, deleted: bool) -> StoredRow {
    let mut values = key_values(key);
    values.push(amount.map_or(Value::Null, Value::Int64));
    values.push(Value::Utf8(label(amount)));
    StoredRow::new(key.clone(), values, version, deleted)
}

const BASE_ROWS: u64 = 1_000;
const BASES: u64 = 4;

struct Table {
    _directory: tempfile::TempDir,
    shape: Shape,
    store: TableStore,
    model: BTreeMap<PrimaryKey, Option<i64>>,
    version: u64,
    last: u64,
}

type Shown = Vec<(Vec<Value>, Option<i64>, String)>;

impl Table {
    fn new(shape: Shape) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let options = StoreOptions {
            background_compaction: false,
            block_rows: 128,
            ..StoreOptions::default()
        };
        let mut store = TableStore::open(directory.path(), shape.schema(), options).unwrap();
        // Every third id is left out, so later inserts land between keys.
        let model = (1..=BASES * BASE_ROWS * 3 / 2)
            .filter(|id| id % 3 != 0)
            .map(|id| (shape.key(id), Some(i64::try_from(id).unwrap())))
            .collect::<BTreeMap<_, _>>();
        let sorted = model.iter().collect::<Vec<_>>();
        for base in sorted.chunks(sorted.len().div_ceil(usize::try_from(BASES).unwrap())) {
            store
                .bulk_ingest_snapshot(
                    base.iter()
                        .map(|(key, amount)| row(key, **amount, 1, false))
                        .collect(),
                )
                .unwrap();
        }
        Self {
            _directory: directory,
            shape,
            store,
            model,
            version: 1,
            last: BASES * BASE_ROWS * 3 / 2,
        }
    }

    /// Changes spread over every key: updates (a NULL among them), deletes,
    /// keys brought back, keys inserted between the stored ones and `fresh`
    /// past the last id.
    fn change(&mut self, number: u64, update: u64, delete: u64, fresh: u64) {
        let mut rows = Vec::new();
        for id in 1..=self.last + fresh {
            let key = self.shape.key(id);
            if id <= self.last && id % delete == number % delete {
                if self.model.remove(&key).is_some() {
                    self.version += 1;
                    rows.push(row(&key, None, self.version, true));
                }
            } else if id > self.last || id % update == number % update {
                let amount =
                    (id % 5 != 0).then(|| i64::try_from(id * 31 + number).unwrap() % 10_007);
                self.version += 1;
                rows.push(row(&key, amount, self.version, false));
                self.model.insert(key, amount);
            }
        }
        self.last += fresh;
        for batch in rows.chunks(500) {
            self.store.ingest_cdc(batch.to_vec()).unwrap();
        }
    }

    fn shown(&self, values: &[Value]) -> (Vec<Value>, Option<i64>, String) {
        let parts = self.shape.key_ids().len();
        let amount = match &values[parts] {
            Value::Int64(amount) => Some(*amount),
            Value::Null => None,
            other => panic!("unexpected amount {other:?}"),
        };
        let Value::Utf8(label) = &values[parts + 1] else {
            panic!("unexpected label {:?}", values[parts + 1]);
        };
        (values[..parts].to_vec(), amount, label.clone())
    }

    fn expected(&self, start: &PrimaryKey, end: &PrimaryKey) -> Shown {
        self.model
            .range(start.clone()..=end.clone())
            .map(|(key, amount)| (key_values(key), *amount, label(*amount)))
            .collect()
    }

    /// The rows of a key range read through the projected stream with the
    /// key's columns named, as the executor reads them.
    fn overlay_scan(&self, start: &PrimaryKey, end: &PrimaryKey) -> Shown {
        let snapshot = self.store.snapshot();
        let ids = (1..=u32::try_from(self.shape.key_ids().len()).unwrap() + 2).collect::<Vec<_>>();
        let mut stream = snapshot
            .scan_projected_range_stream_unbuffered(start, end, &ids, &[])
            .unwrap();
        stream.enable_memtable_overlay(&self.shape.key_ids());
        assert!(
            stream.memtable_overlay_key().is_some(),
            "{:?}: the key's columns were refused",
            self.shape
        );
        let mut rows = Vec::new();
        while let Some(chunk) = stream.next_chunk(usize::MAX).unwrap() {
            for values in chunk.into_rows() {
                rows.push(self.shown(&values));
            }
        }
        rows
    }

    fn assert_exact(&self, state: &str) {
        let shape = self.shape;
        let snapshot = self.store.snapshot();
        let (start, end) = snapshot.key_bounds().expect("rows");
        let expected = self.expected(&start, &end);
        assert_eq!(
            self.overlay_scan(&start, &end),
            expected,
            "{shape:?}: overlay scan, {state}"
        );
        let merged = snapshot
            .scan()
            .unwrap()
            .into_iter()
            .map(|row| self.shown(row.values()))
            .collect::<Vec<_>>();
        assert_eq!(merged, expected, "{shape:?}: row-wise scan, {state}");
        // Ranges that start and end inside segments, between stored keys
        // and on them, and single keys - present, deleted and never stored.
        let keys = self.model.keys().collect::<Vec<_>>();
        for (from, to) in [
            (7, 9),
            (keys.len() / 3, keys.len() / 2),
            (0, keys.len() - 1),
        ] {
            let (start, end) = (keys[from], keys[to]);
            assert_eq!(
                self.overlay_scan(start, end),
                self.expected(start, end),
                "{shape:?}: range {from}..={to}, {state}"
            );
        }
        for id in (1..=self.last + 5).step_by(97) {
            let key = shape.key(id);
            assert_eq!(
                self.overlay_scan(&key, &key),
                self.expected(&key, &key),
                "{shape:?}: key of id {id}, {state}"
            );
        }
    }
}

#[test]
fn text_binary_and_composite_keys_take_the_overlay_and_the_layer() {
    for shape in Shape::ALL {
        let mut table = Table::new(shape);
        table.assert_exact("bases alone");
        // The memtable over the bases.
        table.change(1, 7, 11, 300);
        table.assert_exact("changes in the memtable");
        // The same changes flushed: newer segments over the bases, read
        // through their key index.
        table.store.flush().unwrap();
        table.assert_exact("one flush");
        assert!(
            table.store.metrics().unwrap().layer_index_bytes() > 0,
            "{shape:?}: the flushed changes were read row by row"
        );
        // A second flush over the first, then the memtable over both.
        table.change(2, 5, 13, 200);
        table.store.flush().unwrap();
        table.assert_exact("two flushes");
        table.change(3, 3, 17, 100);
        table.assert_exact("two flushes under the memtable");
        // Folded back into bases.
        let mut passes = 0;
        while table.store.compact().unwrap().input_segments() > 0 {
            passes += 1;
            assert!(passes < 64, "{shape:?}: compaction does not settle");
        }
        table.assert_exact("compacted");
    }
}

/// Keys that are one row under a case- or accent-insensitive collation are
/// different keys here, as they are different rows of the source at
/// different times: a key rewritten in another case arrives as a delete of
/// the old spelling and an insert of the new.
#[test]
fn a_text_key_respelled_in_another_case_replaces_the_old_spelling() {
    let directory = tempfile::tempdir().unwrap();
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut store = TableStore::open(directory.path(), Shape::Text.schema(), options).unwrap();
    let key = |text: &str| PrimaryKey::new(vec![KeyPart::Utf8(text.to_owned())]).unwrap();
    store
        .bulk_ingest_snapshot(
            ["alpha", "beta", "gamma", "resume"]
                .iter()
                .map(|text| row(&key(text), Some(1), 1, false))
                .collect(),
        )
        .unwrap();
    store
        .ingest_cdc(vec![
            row(&key("beta"), None, 2, true),
            row(&key("Beta"), Some(2), 2, false),
            row(&key("resume"), None, 3, true),
            row(&key("r\u{e9}sum\u{e9}"), Some(3), 3, false),
            row(&key("gamma "), Some(4), 4, false),
        ])
        .unwrap();
    let snapshot = store.snapshot();
    let (start, end) = snapshot.key_bounds().expect("rows");
    let mut stream = snapshot
        .scan_projected_range_stream_unbuffered(&start, &end, &[1, 2], &[])
        .unwrap();
    stream.enable_memtable_overlay(&[1]);
    let mut rows = Vec::new();
    while let Some(chunk) = stream.next_chunk(usize::MAX).unwrap() {
        rows.extend(chunk.into_rows());
    }
    let text = |text: &str| Value::Utf8(text.to_owned());
    assert_eq!(
        rows,
        vec![
            vec![text("Beta"), Value::Int64(2)],
            vec![text("alpha"), Value::Int64(1)],
            vec![text("gamma"), Value::Int64(1)],
            vec![text("gamma "), Value::Int64(4)],
            vec![text("r\u{e9}sum\u{e9}"), Value::Int64(3)],
        ]
    );
}
