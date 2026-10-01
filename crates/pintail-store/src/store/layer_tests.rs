//! A table of several base segments taking changes across its whole key
//! range, flushed and compacted under a row budget far smaller than the
//! table: the state between any two passes is read through the layered scan
//! and through the row-wise one, and both must show the model's rows.

use super::*;
use pintail_types::{Column, DataType, Value};
use std::collections::BTreeMap;

const BASE_ROWS: u64 = 1_000;
const BASES: u64 = 6;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "amount", DataType::Int64, true),
            Column::new(3, "label", DataType::Utf8, false),
        ],
    )
    .unwrap()
}

fn row(id: u64, amount: Option<i64>, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).unwrap(),
        vec![
            Value::UInt64(id),
            amount.map_or(Value::Null, Value::Int64),
            Value::Utf8(format!("label {}", amount.unwrap_or(-1) % 9)),
        ],
        version,
        deleted,
    )
}

struct Table {
    _directory: tempfile::TempDir,
    store: TableStore,
    model: BTreeMap<u64, Option<i64>>,
    version: u64,
    last: u64,
}

impl Table {
    fn new(options: StoreOptions) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut store = TableStore::open(directory.path(), schema(), options).unwrap();
        let mut model = BTreeMap::new();
        for base in 0..BASES {
            let ids = base * BASE_ROWS + 1..=(base + 1) * BASE_ROWS;
            store
                .bulk_ingest_snapshot(
                    ids.clone()
                        .map(|id| row(id, Some(i64::try_from(id).unwrap()), 1, false))
                        .collect(),
                )
                .unwrap();
            model.extend(ids.map(|id| (id, Some(i64::try_from(id).unwrap()))));
        }
        Self {
            _directory: directory,
            store,
            model,
            version: 1,
            last: BASES * BASE_ROWS,
        }
    }

    /// Changes spread over every key: each `update`th row rewritten (or
    /// brought back), each `delete`th deleted, `fresh` rows added past the
    /// last key, a NULL among the new values.
    fn change(&mut self, number: u64, update: u64, delete: u64, fresh: u64) {
        let mut rows = Vec::new();
        for id in 1..=self.last {
            if id % delete == number % delete {
                if self.model.remove(&id).is_some() {
                    self.version += 1;
                    rows.push(row(id, None, self.version, true));
                }
            } else if id % update == number % update {
                let amount =
                    (id % 5 != 0).then(|| i64::try_from(id * 31 + number).unwrap() % 10_007);
                self.version += 1;
                rows.push(row(id, amount, self.version, false));
                self.model.insert(id, amount);
            }
        }
        for id in self.last + 1..=self.last + fresh {
            let amount = Some(i64::try_from(id + number).unwrap());
            self.version += 1;
            rows.push(row(id, amount, self.version, false));
            self.model.insert(id, amount);
        }
        self.last += fresh;
        for batch in rows.chunks(500) {
            self.store.ingest_cdc(batch.to_vec()).unwrap();
        }
    }

    /// The table read through the projected stream with its key named, as
    /// the executor reads it.
    fn layered_scan(&self) -> Vec<(u64, Option<i64>, String)> {
        let snapshot = self.store.snapshot();
        let Some((start, end)) = snapshot.key_bounds() else {
            return Vec::new();
        };
        let mut stream = snapshot
            .scan_projected_range_stream_unbuffered(&start, &end, &[1, 2, 3], &[])
            .unwrap();
        stream.enable_memtable_overlay(&[1]);
        let mut rows = Vec::new();
        while let Some(chunk) = stream.next_chunk(usize::MAX).unwrap() {
            for values in chunk.into_rows() {
                let [Value::UInt64(id), amount, Value::Utf8(label)] = values.as_slice() else {
                    panic!("unexpected row {values:?}");
                };
                let amount = match amount {
                    Value::Int64(amount) => Some(*amount),
                    Value::Null => None,
                    other => panic!("unexpected amount {other:?}"),
                };
                rows.push((*id, amount, label.clone()));
            }
        }
        rows
    }

    fn assert_exact(&self, state: &str) {
        let expected = self
            .model
            .iter()
            .map(|(id, amount)| (*id, *amount, format!("label {}", amount.unwrap_or(-1) % 9)))
            .collect::<Vec<_>>();
        assert_eq!(self.layered_scan(), expected, "layered scan, {state}");
        let merged = self
            .store
            .snapshot()
            .scan()
            .unwrap()
            .into_iter()
            .map(|row| {
                let [Value::UInt64(id), amount, Value::Utf8(label)] = row.values() else {
                    panic!("unexpected row");
                };
                let amount = match amount {
                    Value::Int64(amount) => Some(*amount),
                    _ => None,
                };
                (*id, amount, label.clone())
            })
            .collect::<Vec<_>>();
        assert_eq!(merged, expected, "row-wise scan, {state}");
    }

    fn disjoint(&self) -> bool {
        self.store
            .snapshot()
            .grouped_fold_spans(usize::MAX)
            .is_some()
    }
}

#[test]
fn flushed_changes_over_many_bases_are_read_in_place() {
    let mut table = Table::new(StoreOptions::default());
    table.change(1, 7, 11, 300);
    table.store.flush().unwrap();
    table.assert_exact("one flush");
    assert!(
        table.store.metrics().unwrap().layer_index_bytes() > 0,
        "the flushed changes were read row by row"
    );
    // A second flush changing rows the first changed, then the memtable
    // over both.
    table.change(2, 5, 13, 200);
    table.store.flush().unwrap();
    table.assert_exact("two flushes");
    table.change(3, 3, 17, 100);
    table.assert_exact("two flushes under the memtable");
}

#[test]
fn a_cluster_larger_than_the_budget_is_folded_a_key_range_at_a_time() {
    let mut table = Table::new(StoreOptions {
        background_compaction: false,
        // Two bases beside a round's changes; never the cluster whole.
        max_compaction_input_rows: 3_600,
        max_compaction_rows: 1_500,
        ..StoreOptions::default()
    });
    let mut layered = false;
    for round in 1..=3_u64 {
        table.change(round, 6 + round, 10 + round, 150);
        table.store.flush().unwrap();
        table.assert_exact(&format!("round {round} flushed"));
        // One pass, then more changes over the half-folded table.
        let outcome = table.store.compact().unwrap();
        assert!(
            outcome.input_segments() > 0,
            "round {round} planned no merge"
        );
        table.assert_exact(&format!("round {round} after one pass"));
        layered |= table.store.metrics().unwrap().layer_index_bytes() > 0;
    }
    let mut passes = 0;
    while table.store.compact().unwrap().input_segments() > 0 {
        passes += 1;
        assert!(passes < 64, "compaction does not settle");
        table.assert_exact(&format!("pass {passes}"));
    }
    assert!(layered, "no state between passes was read in place");
    assert!(
        table.disjoint(),
        "the changes were not folded into the bases: segments still overlap"
    );
    let tombstones = table
        .store
        .manifest
        .segments
        .iter()
        .filter(|meta| !meta.unique_keys)
        .count();
    assert_eq!(tombstones, 0, "a folded table kept deletes");
}
