//! The fused join-aggregate on a star join whose column types match the
//! benchmark's own schema rather than a two-column stand-in: an unsigned
//! 32-bit dimension key beside text and datetime columns, a fact table of
//! eleven columns with an unsigned 64-bit key, two decimal scales, text,
//! a date and datetimes, and fact rows both in segments and in the
//! memtable. Each answer is checked against a sum computed here, and the
//! join's profile line names the fold that produced it.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const BUYERS: u64 = 2_000;
/// Past the size below which a range with memtable rows is materialized
/// whole rather than streamed.
const STREAMED_PURCHASES: u64 = 80_000;
const SMALL_PURCHASES: u64 = 20_000;
const MEMTABLE_PURCHASES: u64 = 700;
const REGIONS: [&str; 8] = [
    "north", "south", "east", "west", "inner", "outer", "upper", "lower",
];

fn decimal(precision: u8, scale: u8) -> DataType {
    DataType::Decimal { precision, scale }
}

fn buyer_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt32, false),
            Column::new(2, "name", DataType::Utf8, false),
            Column::new(3, "email", DataType::Utf8, false),
            Column::new(4, "region", DataType::Utf8, false),
            Column::new(5, "created_at", DataType::DateTime64 { fsp: 0 }, false),
            Column::new(6, "updated_at", DataType::DateTime64 { fsp: 0 }, false),
        ],
    )
    .expect("schema")
}

fn purchase_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "buyer_id", DataType::UInt32, false),
            Column::new(3, "item_id", DataType::UInt32, false),
            Column::new(4, "quantity", DataType::UInt32, false),
            Column::new(5, "unit_price", decimal(10, 2), false),
            Column::new(6, "total_amount", decimal(12, 2), false),
            Column::new(7, "status", DataType::Utf8, false),
            Column::new(8, "region", DataType::Utf8, false),
            Column::new(9, "placed_on", DataType::Date32, false),
            Column::new(10, "created_at", DataType::DateTime64 { fsp: 0 }, false),
            Column::new(11, "updated_at", DataType::DateTime64 { fsp: 0 }, false),
        ],
    )
    .expect("schema")
}

fn buyer_region(id: u64) -> &'static str {
    REGIONS[usize::try_from(id % 8).expect("small")]
}

/// Every key lands on a buyer: the dimension's key range is dense.
fn purchase_buyer(id: u64) -> u64 {
    1 + (id * 17) % BUYERS
}

fn purchase_cents(id: u64) -> i64 {
    i64::try_from(1_000 + (id * 7_919) % 99_000).expect("small")
}

fn cents_text(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

fn purchase_row(id: u64, sequence: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::UInt64(purchase_buyer(id)),
            Value::UInt64(1 + id % 500),
            Value::UInt64(1 + id % 5),
            Value::Utf8(cents_text(purchase_cents(id) / 2)),
            Value::Utf8(cents_text(purchase_cents(id))),
            Value::Utf8(
                ["pending", "shipped", "delivered"][usize::try_from(id % 3).expect("small")]
                    .to_owned(),
            ),
            Value::Utf8(buyer_region(id + 3).to_owned()),
            Value::Utf8(format!("2024-{:02}-{:02}", 1 + id % 12, 1 + id % 28)),
            Value::Utf8("2024-05-06 07:08:09".to_owned()),
            Value::Utf8("2024-05-06 07:08:09".to_owned()),
        ],
        sequence,
        false,
    )
}

struct Fixture {
    _directory: tempfile::TempDir,
    stores: Vec<TableStore>,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new(segment_purchases: u64) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut purchases = TableStore::open(
            directory.path().join("purchases"),
            purchase_schema(),
            StoreOptions::default(),
        )
        .expect("purchases");
        purchases
            .bulk_ingest_snapshot(
                (1..=segment_purchases)
                    .map(|id| purchase_row(id, 1))
                    .collect(),
            )
            .expect("ingest purchases");
        // Rows replicated after the snapshot stay in the memtable.
        purchases
            .ingest_cdc(
                (segment_purchases + 1..=segment_purchases + MEMTABLE_PURCHASES)
                    .map(|id| purchase_row(id, 2))
                    .collect(),
            )
            .expect("ingest memtable purchases");
        let mut buyers = TableStore::open(
            directory.path().join("buyers"),
            buyer_schema(),
            StoreOptions::default(),
        )
        .expect("buyers");
        buyers
            .bulk_ingest_snapshot(
                (1..=BUYERS)
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            vec![
                                Value::UInt64(id),
                                Value::Utf8(format!("buyer {id}")),
                                Value::Utf8(format!("buyer{id}@example.test")),
                                Value::Utf8(buyer_region(id).to_owned()),
                                Value::Utf8("2023-01-02 03:04:05".to_owned()),
                                Value::Utf8("2023-01-02 03:04:05".to_owned()),
                            ],
                            1,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("ingest buyers");
        let entries = [
            (
                "purchases",
                purchase_schema(),
                segment_purchases + MEMTABLE_PURCHASES,
            ),
            ("buyers", buyer_schema(), BUYERS),
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
        let catalog =
            CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", entries).expect("database")
            ])
            .expect("catalog");
        Self {
            _directory: directory,
            stores: vec![purchases, buyers],
            catalog,
        }
    }

    fn run(&self, sql: &str) -> (Vec<String>, String) {
        let snapshots: Vec<_> = self.stores.iter().map(TableStore::snapshot).collect();
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
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start_profiled(plan, &provider, 1 << 31, None, Collation::default())
                .expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                rows.push(
                    (0..batch.columns().len())
                        .map(|column| {
                            let value = batch
                                .column(column)
                                .and_then(|column| column.value_owned(row))
                                .expect("value");
                            value
                                .text()
                                .map_or_else(|| format!("{value:?}"), str::to_owned)
                        })
                        .collect::<Vec<_>>()
                        .join("|"),
                );
            }
        }
        let profile = execution
            .profile()
            .map(|profile| profile.render())
            .unwrap_or_default();
        (rows, profile)
    }
}

fn join_line(profile: &str) -> &str {
    profile
        .lines()
        .find(|line| line.trim_start().starts_with("HashJoin"))
        .unwrap_or_else(|| panic!("no join line:\n{profile}"))
}

/// Region, count and sum of the purchases, computed without the engine.
fn expected_by_region(segment_purchases: u64) -> Vec<(&'static str, u64, i64)> {
    let mut totals = std::collections::BTreeMap::<&str, (u64, i64)>::new();
    for id in 1..=segment_purchases + MEMTABLE_PURCHASES {
        let entry = totals.entry(buyer_region(purchase_buyer(id))).or_default();
        entry.0 += 1;
        entry.1 += purchase_cents(id);
    }
    totals
        .into_iter()
        .map(|(region, (count, cents))| (region, count, cents))
        .collect()
}

/// The benchmark's join shape over segments with memtable rows past their
/// keys: the segments stream decoded and the memtable rows arrive as row
/// values, and every morsel of both takes the column fold.
#[test]
fn the_benchmark_join_shape_takes_the_column_fold() {
    assert_column_fold(STREAMED_PURCHASES);
}

/// A table small enough that a scan with memtable rows materializes it
/// whole: every batch is rebuilt from row values, whose decimals parse to
/// wide units, and the column fold still takes them.
#[test]
fn a_materialized_small_table_takes_the_column_fold() {
    assert_column_fold(SMALL_PURCHASES);
}

fn assert_column_fold(segment_purchases: u64) {
    let fixture = Fixture::new(segment_purchases);
    let (rows, profile) = fixture.run(
        "SELECT b.region, COUNT(*) AS cnt, ROUND(SUM(p.total_amount), 2) AS total \
         FROM purchases p JOIN buyers b ON p.buyer_id = b.id GROUP BY b.region ORDER BY b.region",
    );
    let expected: Vec<String> = expected_by_region(segment_purchases)
        .into_iter()
        .map(|(region, count, cents)| format!("{region}|UInt64({count})|{}", cents_text(cents)))
        .collect();
    assert_eq!(rows, expected, "{profile}");
    let line = join_line(&profile);
    let morsels = line
        .split("column fold on ")
        .nth(1)
        .unwrap_or_else(|| panic!("no column fold:\n{profile}"));
    let mut counts = morsels
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty());
    let (folded, total) = (counts.next(), counts.next());
    assert!(
        folded.is_some() && folded == total && folded != Some("0"),
        "not every morsel took the column fold:\n{profile}"
    );
    // Decimal sums and counts add up in any order, so each worker keeps one
    // set of totals for the whole probe.
    assert!(line.contains("totals kept per worker"), "{profile}");
}

/// A sum of an integer column folds a column at a time too, on segment and
/// memtable rows alike, and answers what the unfused join over an
/// expression does.
#[test]
fn an_integer_sum_takes_the_column_fold() {
    for purchases in [STREAMED_PURCHASES, SMALL_PURCHASES] {
        let fixture = Fixture::new(purchases);
        let sql = |extra: &str| {
            format!(
                "SELECT b.region, COUNT(*), SUM(p.quantity), SUM(p.total_amount){extra} \
                 FROM purchases p JOIN buyers b ON p.buyer_id = b.id \
                 GROUP BY b.region ORDER BY b.region"
            )
        };
        let (rows, profile) = fixture.run(&sql(""));
        // A DISTINCT aggregate keeps the join out of the fused aggregate,
        // so the reference sums through the general operator.
        let (reference, reference_profile) = fixture.run(&sql(", COUNT(DISTINCT p.item_id)"));
        assert!(
            join_line(&reference_profile).contains("not fused"),
            "{reference_profile}"
        );
        let reference = reference
            .into_iter()
            .map(|row| row.rsplit_once('|').expect("a distinct count").0.to_owned())
            .collect::<Vec<_>>();
        assert_eq!(rows, reference, "{profile}");
        let line = join_line(&profile);
        assert!(
            line.contains("column fold on") && !line.contains("row fold"),
            "{profile}"
        );
        // An integer sum's total is exact in 128 bits and joins the state
        // as it is, so it is carried across morsels like any other lane.
        assert!(line.contains("totals kept per worker"), "{profile}");
    }
}

/// An aggregate the column fold has no lane for keeps the fused join and
/// says why it took the row fold.
#[test]
fn an_aggregate_without_a_lane_names_the_row_fold() {
    let fixture = Fixture::new(SMALL_PURCHASES);
    let (rows, profile) = fixture.run(
        "SELECT b.region, COUNT(*), MAX(p.total_amount) \
         FROM purchases p JOIN buyers b ON p.buyer_id = b.id GROUP BY b.region ORDER BY b.region",
    );
    assert_eq!(rows.len(), REGIONS.len());
    assert!(
        join_line(&profile).contains("row fold: an aggregate without a column lane"),
        "{profile}"
    );
}

/// A shape the fused join declines says so on the join's own line.
#[test]
fn a_declined_fusion_names_its_reason() {
    let fixture = Fixture::new(SMALL_PURCHASES);
    let (rows, profile) = fixture.run(
        "SELECT b.region, COUNT(DISTINCT p.item_id) \
         FROM purchases p JOIN buyers b ON p.buyer_id = b.id GROUP BY b.region ORDER BY b.region",
    );
    assert_eq!(rows.len(), REGIONS.len());
    let line = join_line(&profile);
    assert!(
        line.contains("not fused into the aggregate: a DISTINCT or concatenating aggregate")
            && !line.contains(" rows=0 "),
        "{profile}"
    );
}
