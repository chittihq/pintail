//! A small dimension table that is being written to: its rows sit in
//! segments and its newest changes - updates, deletes and inserts - in the
//! memtable, the state every replica of a live source answers from.
//!
//! Each answer is checked against the same final rows loaded whole into a
//! second store with no memtable, so the scan's visibility (newest version
//! wins, deletes hide, inserts appear, a column added after the segments
//! were written reads NULL there) is asserted, not assumed.
//!
//! `memtable_dimension_scan_cost` is `#[ignore]`d: measurement, not
//! assertion. Run with `PINTAIL_DISABLE_SETTLED_MEMO=1 cargo test --profile
//! recovery -p pintail-exec --test integration memtable_dimension_scan:: --
//! --ignored --nocapture`; `DIMENSION_SIZES`, `DIMENSION_PERCENTS`,
//! `DIMENSION_QUERY` and `DIMENSION_RUNS` narrow it, and `DIMENSION_PROFILE=1`
//! prints each run's operator profile.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ORDERS: u64 = 400_000;
const REGIONS: [&str; 6] = ["north", "south", "east", "west", "coast", "inland"];

fn shop_schema(altered: bool) -> TableSchema {
    let mut columns = vec![
        Column::new(1, "id", DataType::UInt64, false),
        Column::new(2, "region", DataType::Utf8, false),
        Column::new(3, "label", DataType::Utf8, false),
        Column::new(4, "rate", DataType::Int64, false),
    ];
    if altered {
        columns.push(Column::new(5, "grade", DataType::Int64, true));
    }
    TableSchema::new(if altered { 2 } else { 1 }, columns).expect("schema")
}

fn order_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "shop_id", DataType::UInt64, false),
            Column::new(3, "cents", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

#[derive(Clone)]
struct Shop {
    region: &'static str,
    label: String,
    rate: i64,
    grade: Option<i64>,
}

impl Shop {
    fn original(id: u64) -> Self {
        Self {
            region: REGIONS[usize::try_from(id % 6).expect("small")],
            label: format!("shop {id:08}"),
            rate: i64::try_from(id % 1_000).expect("small"),
            grade: None,
        }
    }

    fn revised(id: u64) -> Self {
        Self {
            region: REGIONS[usize::try_from((id + 1) % 6).expect("small")],
            label: format!("shop {id:08} revised"),
            rate: i64::try_from(id % 997).expect("small") + 5_000,
            grade: (!id.is_multiple_of(3)).then(|| i64::try_from(id % 11).expect("small")),
        }
    }

    fn row(&self, id: u64, altered: bool, version: u64, deleted: bool) -> StoredRow {
        let mut values = vec![
            Value::UInt64(id),
            Value::Utf8(self.region.to_owned()),
            Value::Utf8(self.label.clone()),
            Value::Int64(self.rate),
        ];
        if altered {
            values.push(self.grade.map_or(Value::Null, Value::Int64));
        }
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
            values,
            version,
            deleted,
        )
    }
}

fn order_row(id: u64, shops: u64) -> StoredRow {
    let signed = i64::try_from(id).expect("small");
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::UInt64(id.wrapping_mul(7_919) % shops + 1),
            Value::Int64(signed.wrapping_mul(31) % 10_000),
        ],
        1,
        false,
    )
}

struct Fixture {
    _directory: tempfile::TempDir,
    /// Shops in segments with changes in the memtable, then orders.
    live: Vec<TableStore>,
    /// The same final shops loaded whole, then the same orders.
    settled: Vec<TableStore>,
    catalog: CatalogSnapshot,
    updated: u64,
    /// The highest key, inserted through the change path when any was.
    last: u64,
}

impl Fixture {
    /// `shops` rows loaded as a snapshot, then `percent` of them changed
    /// through the change path - a third updated, a third deleted, a third
    /// inserted past the last key - and left in the memtable. `altered`
    /// adds a nullable column between the snapshot and the changes.
    fn new(shops: u64, percent: u64, altered: bool) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let open = |name: &str, schema: TableSchema| {
            TableStore::open(directory.path().join(name), schema, StoreOptions::default())
                .expect("store")
        };
        let mut model = (1..=shops)
            .map(|id| (id, Shop::original(id)))
            .collect::<BTreeMap<_, _>>();
        let mut live = open("live-shops", shop_schema(false));
        live.bulk_ingest_snapshot(
            model
                .iter()
                .map(|(id, shop)| shop.row(*id, false, 1, false))
                .collect(),
        )
        .expect("snapshot");
        if altered {
            live.evolve_schema(shop_schema(true)).expect("evolve");
        }
        let each = shops * percent / 300;
        let mut changes = Vec::new();
        let mut version = 2;
        let mut updated = 1;
        let mut last = shops;
        if let Some(step) = shops.checked_div(each) {
            for index in 0..each {
                let id = 1 + index * step;
                updated = id;
                let shop = Shop::revised(id);
                changes.push(shop.row(id, altered, version, false));
                model.insert(id, shop);
                version += 1;
                let gone = 2 + index * step;
                changes.push(Shop::original(gone).row(gone, altered, version, true));
                model.remove(&gone);
                version += 1;
                let fresh = shops + 1 + index;
                last = fresh;
                let shop = Shop::revised(fresh);
                changes.push(shop.row(fresh, altered, version, false));
                model.insert(fresh, shop);
                version += 1;
            }
        }
        for batch in changes.chunks(2_000) {
            live.ingest_cdc(batch.to_vec()).expect("changes");
        }
        let mut settled = open("settled-shops", shop_schema(altered));
        settled
            .bulk_ingest_snapshot(
                model
                    .iter()
                    .map(|(id, shop)| shop.row(*id, altered, 1, false))
                    .collect(),
            )
            .expect("settled");
        let key_space = shops + each;
        let orders = |name: &str| {
            let mut store = open(name, order_schema());
            store
                .bulk_ingest_snapshot((1..=ORDERS).map(|id| order_row(id, key_space)).collect())
                .expect("orders");
            store
        };
        let live = vec![live, orders("live-orders")];
        let settled = vec![settled, orders("settled-orders")];
        let entries = [
            ("shops", shop_schema(altered), shops),
            ("orders", order_schema(), ORDERS),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (name, schema, rows))| {
            TableEntry::new(
                TableId::new(u64::try_from(index + 1).expect("table")),
                name,
                schema,
                TableStatistics::with_row_count(rows),
            )
            .expect("entry")
            .with_key_columns([1])
            .expect("key")
        })
        .collect::<Vec<_>>();
        Self {
            _directory: directory,
            live,
            settled,
            catalog: CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", entries).expect("database")
            ])
            .expect("catalog"),
            updated,
            last,
        }
    }

    fn run(&self, stores: &[TableStore], sql: &str) -> (Vec<Vec<Value>>, f64) {
        let snapshots = stores.iter().map(TableStore::snapshot).collect::<Vec<_>>();
        let provider = SnapshotScanProvider::new(snapshots.iter().enumerate().map(|(i, s)| {
            (
                DatabaseId::new(1),
                TableId::new(u64::try_from(i + 1).expect("table")),
                s,
            )
        }))
        .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let started = std::time::Instant::now();
        let profiled = std::env::var_os("DIMENSION_PROFILE").is_some();
        let mut execution = if profiled {
            Execution::start_profiled(physical, &provider, 1 << 30, None, Collation::default())
        } else {
            Execution::start(physical, &provider, 1 << 30, Collation::default())
        }
        .expect("start");
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
        if profiled && let Some(profile) = execution.profile() {
            println!("{}", profile.render());
        }
        (rows, started.elapsed().as_secs_f64() * 1000.0)
    }

    fn queries(&self, altered: bool) -> Vec<(&'static str, String)> {
        let mut queries = vec![
            (
                "full scan",
                "SELECT COUNT(*), SUM(rate), MAX(label), MIN(region) FROM shops".to_owned(),
            ),
            (
                "filtered scan",
                "SELECT COUNT(*), SUM(rate), MAX(label) FROM shops WHERE region = 'north'"
                    .to_owned(),
            ),
            (
                "rows out",
                "SELECT id, label FROM shops WHERE rate BETWEEN 998 AND 5010 ORDER BY id"
                    .to_owned(),
            ),
            (
                "star join",
                "SELECT s.region, COUNT(*), SUM(o.cents) FROM orders o JOIN shops s \
                 ON o.shop_id = s.id GROUP BY s.region ORDER BY s.region"
                    .to_owned(),
            ),
            (
                "point",
                format!("SELECT label, rate FROM shops WHERE id = {}", self.updated),
            ),
        ];
        if altered {
            queries.push((
                "added column",
                "SELECT COUNT(grade), SUM(grade), COUNT(*) FROM shops WHERE grade IS NULL OR grade > 3"
                    .to_owned(),
            ));
        }
        queries
    }

    fn assert_exact(&self, altered: bool) {
        for (label, sql) in self.queries(altered) {
            let (live, _) = self.run(&self.live, &sql);
            let (settled, _) = self.run(&self.settled, &sql);
            assert_eq!(live, settled, "{label}: {sql}");
            assert!(!settled.is_empty(), "{label}: {sql}");
        }
        // Point lookups of an updated key, a deleted one, one the snapshot
        // left alone, the last inserted one and one past every key.
        for id in [
            self.updated,
            self.updated + 1,
            self.updated + 2,
            self.last,
            self.last + 1,
        ] {
            let sql = format!("SELECT id, label, rate FROM shops WHERE id = {id}");
            assert_eq!(
                self.run(&self.live, &sql).0,
                self.run(&self.settled, &sql).0,
                "{sql}"
            );
        }
    }
}

#[test]
fn a_written_dimension_answers_exactly() {
    for shops in [5_000, 70_000] {
        for percent in [0, 1, 10] {
            for altered in [false, true] {
                Fixture::new(shops, percent, altered).assert_exact(altered);
            }
        }
    }
}

#[test]
#[ignore = "measurement, not an assertion"]
fn memtable_dimension_scan_cost() {
    let list = |name: &str, default: &[u64]| {
        std::env::var(name).map_or_else(
            |_| default.to_vec(),
            |list| {
                list.split(',')
                    .map(|item| item.parse().expect("number"))
                    .collect::<Vec<u64>>()
            },
        )
    };
    let sizes = list(
        "DIMENSION_SIZES",
        &[5_000, 20_000, 60_000, 200_000, 2_000_000],
    );
    let percents = list("DIMENSION_PERCENTS", &[0, 1, 10]);
    let runs = usize::try_from(list("DIMENSION_RUNS", &[9])[0]).expect("runs");
    let only = std::env::var("DIMENSION_QUERY").ok();
    for shops in sizes {
        for percent in percents.iter().copied() {
            let fixture = Fixture::new(shops, percent, false);
            fixture.assert_exact(false);
            for (label, sql) in fixture.queries(false) {
                if only.as_deref().is_some_and(|only| only != label) {
                    continue;
                }
                // The settled store holds the same final rows with nothing in
                // the memtable: what the written table's answer should cost.
                // The two arms alternate so neither runs on a warmer machine.
                let mut times = Vec::with_capacity(runs);
                let mut settled = Vec::with_capacity(runs);
                for _ in 0..runs {
                    times.push(fixture.run(&fixture.live, &sql).1);
                    settled.push(fixture.run(&fixture.settled, &sql).1);
                }
                times.sort_by(f64::total_cmp);
                settled.sort_by(f64::total_cmp);
                println!(
                    "shops {shops:>9} changed {percent:>2}% {label:<14} median {:>8.2} ms  min {:>8.2} ms  settled median {:>8.2} ms  min {:>8.2} ms  ratio {:>5.2}",
                    times[runs / 2],
                    times[0],
                    settled[runs / 2],
                    settled[0],
                    times[runs / 2] / settled[runs / 2],
                );
            }
        }
    }
}

/// A fact table that is being written to, beside a small settled dimension:
/// the fact's rows sit in segments and `percent` of them changed through the
/// change path, so its scan hands masked and interleaved batches to the
/// aggregate lanes above it.
mod written_fact {
    use super::{
        CatalogSnapshot, Column, DataType, DatabaseEntry, DatabaseId, Fixture, KeyPart, PrimaryKey,
        Shop, StoreOptions, StoredRow, TableEntry, TableId, TableSchema, TableStatistics,
        TableStore, Value, shop_schema,
    };

    const SHOPS: u64 = 5_000;

    fn sale_schema() -> TableSchema {
        TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "shop_id", DataType::UInt64, false),
                Column::new(
                    3,
                    "amount",
                    DataType::Decimal {
                        precision: 12,
                        scale: 2,
                    },
                    false,
                ),
                Column::new(4, "placed", DataType::Date32, false),
                Column::new(5, "qty", DataType::Int64, true),
            ],
        )
        .expect("schema")
    }

    /// The sale `id` as first loaded, or as the change path rewrote it.
    fn sale(id: u64, revised: bool, version: u64, deleted: bool) -> StoredRow {
        let seed = if revised { id.wrapping_mul(13) + 7 } else { id };
        let cents = seed.wrapping_mul(31) % 1_000_000;
        // 2024-01-01 is day 19723.
        let day = 19_723 + i64::try_from(seed % 700).expect("small");
        let qty = (!seed.is_multiple_of(17)).then(|| i64::try_from(seed % 50).expect("small") - 20);
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
            vec![
                Value::UInt64(id),
                Value::UInt64(seed.wrapping_mul(7_919) % SHOPS + 1),
                Value::Utf8(format!("{}.{:02}", cents / 100, cents % 100)),
                Value::Utf8(pintail_types::format_date_days(day).expect("date")),
                qty.map_or(Value::Null, Value::Int64),
            ],
            version,
            deleted,
        )
    }

    /// `sales` rows loaded as a snapshot, then `percent` of them changed - a
    /// third updated, a third deleted, a third inserted past the last key -
    /// and left in the memtable; the settled copy holds the same final rows
    /// in segments alone.
    pub(super) fn fixture(sales: u64, percent: u64) -> Fixture {
        let directory = tempfile::tempdir().expect("directory");
        let open = |name: &str, schema: TableSchema| {
            TableStore::open(directory.path().join(name), schema, StoreOptions::default())
                .expect("store")
        };
        let shops = |name: &str| {
            let mut store = open(name, shop_schema(false));
            store
                .bulk_ingest_snapshot(
                    (1..=SHOPS)
                        .map(|id| Shop::original(id).row(id, false, 1, false))
                        .collect(),
                )
                .expect("shops");
            store
        };
        let each = sales * percent / 300;
        let step = sales.checked_div(each).unwrap_or(0);
        // What the change path did to `id`: updated, deleted, or nothing.
        let updated = |id: u64| each > 0 && (id - 1).is_multiple_of(step) && (id - 1) / step < each;
        let deleted = |id: u64| {
            each > 0 && id >= 2 && (id - 2).is_multiple_of(step) && (id - 2) / step < each
        };
        let load = |store: &mut TableStore, rows: &mut dyn Iterator<Item = StoredRow>| {
            loop {
                let chunk = rows.take(500_000).collect::<Vec<_>>();
                if chunk.is_empty() {
                    break;
                }
                store.bulk_ingest_snapshot(chunk).expect("snapshot");
            }
        };
        let mut live = open("live-sales", sale_schema());
        load(
            &mut live,
            &mut (1..=sales).map(|id| sale(id, false, 1, false)),
        );
        let mut version = 2;
        let mut changes = Vec::new();
        for index in 0..each {
            for (id, revised, gone) in [
                (1 + index * step, true, false),
                (2 + index * step, false, true),
                (sales + 1 + index, true, false),
            ] {
                changes.push(sale(id, revised, version, gone));
                version += 1;
            }
            if changes.len() >= 3_000 {
                live.ingest_cdc(std::mem::take(&mut changes))
                    .expect("changes");
            }
        }
        if !changes.is_empty() {
            live.ingest_cdc(changes).expect("changes");
        }
        let mut settled = open("settled-sales", sale_schema());
        load(
            &mut settled,
            &mut (1..=sales + each)
                .filter(|id| *id > sales || !deleted(*id))
                .map(|id| sale(id, id > sales || updated(id), 1, false)),
        );
        let entries = [
            ("shops", shop_schema(false), SHOPS),
            ("sales", sale_schema(), sales),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (name, schema, rows))| {
            TableEntry::new(
                TableId::new(u64::try_from(index + 1).expect("table")),
                name,
                schema,
                TableStatistics::with_row_count(rows),
            )
            .expect("entry")
            .with_key_columns([1])
            .expect("key")
        })
        .collect::<Vec<_>>();
        let live = vec![shops("live-shops"), live];
        let settled = vec![shops("settled-shops"), settled];
        Fixture {
            _directory: directory,
            live,
            settled,
            catalog: CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", entries).expect("database")
            ])
            .expect("catalog"),
            updated: 1,
            last: sales + each,
        }
    }

    pub(super) fn queries(fixture: &Fixture) -> Vec<(&'static str, String)> {
        vec![
            (
                "fact sums",
                "SELECT COUNT(*), SUM(amount), SUM(qty), COUNT(qty) FROM sales".to_owned(),
            ),
            (
                "fact filter",
                "SELECT COUNT(*), SUM(amount), MIN(placed), MAX(placed) FROM sales \
                 WHERE placed >= '2025-01-01' AND qty > 3"
                    .to_owned(),
            ),
            (
                "fact groups",
                "SELECT placed, COUNT(*), SUM(amount), SUM(qty) FROM sales GROUP BY placed \
                 ORDER BY placed"
                    .to_owned(),
            ),
            (
                "fact join",
                "SELECT s.region, COUNT(*), SUM(o.amount), SUM(o.qty) FROM sales o JOIN shops s \
                 ON o.shop_id = s.id GROUP BY s.region ORDER BY s.region"
                    .to_owned(),
            ),
            (
                "fact join filter",
                "SELECT s.region, COUNT(*), SUM(o.amount), AVG(o.amount) FROM sales o JOIN shops s \
                 ON o.shop_id = s.id WHERE s.rate > 700 AND o.placed < '2025-06-01' \
                 GROUP BY s.region ORDER BY s.region"
                    .to_owned(),
            ),
            (
                "fact rows",
                format!(
                    "SELECT id, shop_id, amount, placed, qty FROM sales WHERE id > {} OR id < 40 \
                     ORDER BY id",
                    fixture.last.saturating_sub(40)
                ),
            ),
        ]
    }
}

#[test]
fn a_written_fact_answers_exactly() {
    for sales in [5_000, 300_000] {
        for percent in [0, 1, 10, 30] {
            let fixture = written_fact::fixture(sales, percent);
            for (label, sql) in written_fact::queries(&fixture) {
                let (live, _) = fixture.run(&fixture.live, &sql);
                let (settled, _) = fixture.run(&fixture.settled, &sql);
                assert_eq!(live, settled, "{sales} rows, {percent}%: {label}: {sql}");
                assert!(!settled.is_empty(), "{label}: {sql}");
            }
        }
    }
}

#[test]
#[ignore = "measurement, not an assertion"]
fn memtable_fact_scan_cost() {
    let list = |name: &str, default: &[u64]| {
        std::env::var(name).map_or_else(
            |_| default.to_vec(),
            |list| {
                list.split(',')
                    .map(|item| item.parse().expect("number"))
                    .collect::<Vec<u64>>()
            },
        )
    };
    let sizes = list("FACT_SIZES", &[200_000, 2_000_000]);
    let percents = list("DIMENSION_PERCENTS", &[0, 1, 10, 30]);
    let runs = usize::try_from(list("DIMENSION_RUNS", &[9])[0]).expect("runs");
    let only = std::env::var("DIMENSION_QUERY").ok();
    for sales in sizes {
        for percent in percents.iter().copied() {
            let fixture = written_fact::fixture(sales, percent);
            for (label, sql) in written_fact::queries(&fixture) {
                if only.as_deref().is_some_and(|only| only != label) {
                    continue;
                }
                assert_eq!(
                    fixture.run(&fixture.live, &sql).0,
                    fixture.run(&fixture.settled, &sql).0,
                    "{label}: {sql}"
                );
                let mut times = Vec::with_capacity(runs);
                let mut settled = Vec::with_capacity(runs);
                for _ in 0..runs {
                    times.push(fixture.run(&fixture.live, &sql).1);
                    settled.push(fixture.run(&fixture.settled, &sql).1);
                }
                times.sort_by(f64::total_cmp);
                settled.sort_by(f64::total_cmp);
                println!(
                    "sales {sales:>9} changed {percent:>2}% {label:<16} median {:>8.2} ms  min {:>8.2} ms  settled median {:>8.2} ms  min {:>8.2} ms  ratio {:>5.2}",
                    times[runs / 2],
                    times[0],
                    settled[runs / 2],
                    settled[0],
                    times[runs / 2] / settled[runs / 2],
                );
            }
        }
    }
}
