//! A table whose changes have reached segments: the bases its first load
//! wrote, newer segments over them from two later flushes - updates, deletes
//! and inserts, the second flush changing rows the first already changed and
//! written after a column was added - and the newest changes in the memtable
//! over both. This is the state a replica of a busy table is in between
//! compactions.
//!
//! Every answer is checked against the same final rows loaded whole into a
//! second store, and the store is asked afterwards whether the scans read
//! the newer segments in place (their key index was resolved) rather than
//! through the row-wise merge.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const SALES: u64 = 150_000;

fn schema(noted: bool, composite: bool) -> TableSchema {
    let mut columns = vec![Column::new(1, "id", DataType::UInt64, false)];
    if composite {
        columns.push(Column::new(2, "part", DataType::Int64, false));
    }
    columns.extend([
        Column::new(3, "shop_id", DataType::UInt64, false),
        Column::new(
            4,
            "amount",
            DataType::Decimal {
                precision: 12,
                scale: 2,
            },
            false,
        ),
        Column::new(5, "placed", DataType::Date32, false),
        Column::new(6, "qty", DataType::Int64, true),
    ]);
    if noted {
        columns.push(Column::new(7, "note", DataType::Utf8, true));
    }
    TableSchema::new(if noted { 2 } else { 1 }, columns).expect("schema")
}

/// What a sale holds: the seed its values derive from and its note.
type Sale = (u64, Option<String>);

fn part_of(id: u64) -> i64 {
    i64::try_from(id % 3).expect("small") - 1
}

fn sale_row(
    id: u64,
    sale: &Sale,
    noted: bool,
    composite: bool,
    version: u64,
    deleted: bool,
) -> StoredRow {
    let (seed, note) = sale;
    let cents = seed.wrapping_mul(31) % 1_000_000;
    // 2024-01-01 is day 19723.
    let day = 19_723 + i64::try_from(seed % 700).expect("small");
    let qty = (!seed.is_multiple_of(17)).then(|| i64::try_from(seed % 50).expect("small") - 20);
    let mut key = vec![KeyPart::UInt64(id)];
    let mut values = vec![Value::UInt64(id)];
    if composite {
        key.push(KeyPart::Int64(part_of(id)));
        values.push(Value::Int64(part_of(id)));
    }
    values.extend([
        Value::UInt64(seed.wrapping_mul(7_919) % 5_000 + 1),
        Value::Utf8(format!("{}.{:02}", cents / 100, cents % 100)),
        Value::Utf8(pintail_types::format_date_days(day).expect("date")),
        qty.map_or(Value::Null, Value::Int64),
    ]);
    if noted {
        values.push(note.clone().map_or(Value::Null, Value::Utf8));
    }
    StoredRow::new(PrimaryKey::new(key).expect("key"), values, version, deleted)
}

struct Fixture {
    _directory: tempfile::TempDir,
    live: TableStore,
    settled: TableStore,
    catalog: CatalogSnapshot,
    last: u64,
}

impl Fixture {
    /// The first load in three base segments, then three rounds of changes:
    /// the first two each flushed to a segment of its own, the third left in
    /// the memtable when `unflushed`. The column `note` is added between the
    /// first flush and the second.
    #[allow(clippy::too_many_lines)]
    fn new(composite: bool, unflushed: bool) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let open = |name: &str, schema: TableSchema| {
            TableStore::open(directory.path().join(name), schema, StoreOptions::default())
                .expect("store")
        };
        let mut model: BTreeMap<u64, Sale> = (1..=SALES).map(|id| (id, (id, None))).collect();
        let mut live = open("live", schema(false, composite));
        for chunk in (1..=SALES).collect::<Vec<_>>().chunks(50_000) {
            live.bulk_ingest_snapshot(
                chunk
                    .iter()
                    .map(|id| sale_row(*id, &(*id, None), false, composite, 1, false))
                    .collect(),
            )
            .expect("snapshot");
        }
        let mut version = 1;
        let mut last = SALES;
        // One round: update every `update`th sale still there (and bring
        // back every one that was deleted), delete every `delete`th, and
        // insert `fresh` past the last key.
        let mut round = |live: &mut TableStore,
                         model: &mut BTreeMap<u64, Sale>,
                         noted: bool,
                         number: u64,
                         update: u64,
                         delete: u64,
                         fresh: u64| {
            let mut changes = Vec::new();
            for id in 1..=last {
                if id % delete == number {
                    if model.remove(&id).is_some() {
                        version += 1;
                        changes.push(sale_row(id, &(id, None), noted, composite, version, true));
                    }
                } else if id % update == number {
                    let sale = (
                        id.wrapping_mul(13) + number,
                        (noted && id % 4 != 0).then(|| format!("n {}", (id + number) % 23)),
                    );
                    version += 1;
                    changes.push(sale_row(id, &sale, noted, composite, version, false));
                    model.insert(id, sale);
                }
            }
            for id in last + 1..=last + fresh {
                let sale = (
                    id.wrapping_mul(7) + number,
                    noted.then(|| format!("n {}", id % 23)),
                );
                version += 1;
                changes.push(sale_row(id, &sale, noted, composite, version, false));
                model.insert(id, sale);
            }
            last += fresh;
            for batch in changes.chunks(3_000) {
                live.ingest_cdc(batch.to_vec()).expect("changes");
            }
        };
        round(&mut live, &mut model, false, 1, 7, 11, 4_000);
        live.flush().expect("first flush");
        live.evolve_schema(schema(true, composite)).expect("evolve");
        round(&mut live, &mut model, true, 2, 5, 13, 3_000);
        live.flush().expect("second flush");
        if unflushed {
            round(&mut live, &mut model, true, 3, 9, 17, 1_500);
        }
        let mut settled = open("settled", schema(true, composite));
        settled
            .bulk_ingest_snapshot(
                model
                    .iter()
                    .map(|(id, sale)| sale_row(*id, sale, true, composite, 1, false))
                    .collect(),
            )
            .expect("settled");
        let entry = TableEntry::new(
            TableId::new(1),
            "sales",
            schema(true, composite),
            TableStatistics::with_row_count(SALES),
        )
        .expect("entry");
        let entry = if composite {
            entry.with_key_columns([1, 2])
        } else {
            entry.with_key_columns([1])
        }
        .expect("key");
        Self {
            _directory: directory,
            live,
            settled,
            catalog: CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
            ])
            .expect("catalog"),
            last,
        }
    }

    fn run(&self, store: &TableStore, sql: &str) -> Vec<Vec<Value>> {
        let snapshot = store.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 1 << 30, Collation::default()).expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value_owned(row).expect("value"))
                        .collect(),
                );
            }
        }
        rows
    }

    fn assert_exact(&self) {
        let mut queries = vec![
            "SELECT COUNT(*), SUM(amount), SUM(qty), COUNT(qty), MIN(placed), MAX(placed), \
             COUNT(note), MAX(note) FROM sales"
                .to_owned(),
            "SELECT COUNT(*), SUM(amount), MIN(placed), MAX(note) FROM sales \
             WHERE placed >= '2025-01-01' AND qty > 3"
                .to_owned(),
            "SELECT id, shop_id, amount, placed, qty, note FROM sales WHERE qty = 7 \
             AND shop_id < 900 ORDER BY id"
                .to_owned(),
            "SELECT placed, COUNT(*), SUM(amount), SUM(qty) FROM sales GROUP BY placed \
             ORDER BY placed"
                .to_owned(),
            "SELECT note, COUNT(*), SUM(amount) FROM sales GROUP BY note ORDER BY note".to_owned(),
            "SELECT COUNT(note), COUNT(*) FROM sales WHERE note IS NULL OR note > 'n 5'".to_owned(),
            "SELECT id, amount, note FROM sales ORDER BY id LIMIT 9".to_owned(),
            "SELECT id, amount, note FROM sales ORDER BY id DESC LIMIT 9".to_owned(),
            format!(
                "SELECT id, shop_id, amount, placed, qty, note FROM sales \
                 WHERE id > {} OR id < 40 ORDER BY id",
                self.last - 40
            ),
            format!(
                "SELECT COUNT(*), SUM(amount) FROM sales WHERE id BETWEEN 49990 AND {}",
                SALES + 10
            ),
        ];
        // A key the first flush updated, the second flush updated again,
        // one each deleted, one both left alone, inserted ones, and one
        // past every key.
        for id in [
            35,
            36,
            7,
            8,
            12,
            15,
            2,
            3,
            SALES + 1,
            SALES + 4_001,
            self.last,
            self.last + 1,
        ] {
            queries.push(format!(
                "SELECT id, amount, placed, qty, note FROM sales WHERE id = {id}"
            ));
        }
        for sql in queries {
            assert_eq!(
                self.run(&self.live, &sql),
                self.run(&self.settled, &sql),
                "{sql}"
            );
        }
    }
}

#[test]
fn newer_segments_over_the_bases_answer_exactly() {
    for composite in [false, true] {
        for unflushed in [false, true] {
            let fixture = Fixture::new(composite, unflushed);
            assert_eq!(
                fixture.live.metrics().expect("metrics").segment_count(),
                5,
                "three bases under two flushes"
            );
            fixture.assert_exact();
            assert!(
                fixture.live.metrics().expect("metrics").layer_index_bytes() > 0,
                "the newer segments were read row by row (composite {composite}, \
                 unflushed {unflushed})"
            );
        }
    }
}
