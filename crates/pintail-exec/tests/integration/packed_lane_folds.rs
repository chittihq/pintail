//! Packed aggregate lanes folded a column at a time - over an integer key's
//! range, and over dense text and text-pair slots - agree with the general
//! path, which an expression key forces.
//!
//! The table is built so one query crosses every transition the range fold
//! has: a first window of narrow keys, later windows that widen the range
//! (the fold re-bases), a stretch of keys too sparse for any array (the fold
//! hands its groups to the partition maps mid-stream), and a return to the
//! first keys. NULL keys, NULL amounts and a group whose amounts are all
//! NULL ride along throughout, and a filter makes the folds read selected
//! rows rather than whole spans.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const SHELVES: [&str; 6] = ["oak", "pine", "birch", "ash", "elm", "fir"];
const AISLES: [&str; 3] = ["north", "south", "east"];

/// The key every row of one stretch carries; `None` is NULL.
fn key_of(id: u64) -> Option<i64> {
    let id_signed = i64::try_from(id).expect("small id");
    if id.is_multiple_of(29) {
        return None;
    }
    Some(match id {
        // Wider on both sides: the range re-bases.
        200_000..400_000 => id_signed % 9_000 - 4_500,
        // Too sparse for an array of any useful width.
        400_000..480_000 => (id_signed * 7_919) % 1_000_000_007,
        // Narrow, 600 keys around zero: first in the range fold, and after
        // the sparse stretch in the partition maps.
        _ => id_signed % 600 - 300,
    })
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

/// One row of the table: every column a function of `id`.
fn stock_row(id: u64, key_type: DataType) -> StoredRow {
    let key = key_of(id);
    let owner = match (key, key_type) {
        (None, _) => Value::Null,
        // Unsigned keys sit above an offset, so they
        // stay positive and the range does not start
        // at zero.
        (Some(key), DataType::UInt64) => {
            Value::UInt64(u64::try_from(key + 2_000_000_000).expect("offset keeps it positive"))
        }
        (Some(key), _) => Value::Int64(key),
    };
    // Key 17 never has an amount, so its SUM, AVG, MIN
    // and MAX stay NULL while its COUNT does not.
    let amount = if id.is_multiple_of(13) || key == Some(17) {
        Value::Null
    } else {
        let cents = i64::try_from((id * 7_919) % 2_000_000).expect("small") - 400_000;
        Value::Utf8(format!(
            "{}{}.{:02}",
            if cents < 0 { "-" } else { "" },
            cents.abs() / 100,
            cents.abs() % 100
        ))
    };
    let shelf = if id.is_multiple_of(31) {
        Value::Null
    } else {
        Value::Utf8(SHELVES[usize::try_from(id % 6).expect("small")].to_owned())
    };
    // About five years of days, starting mid-year, NULL now and then.
    let stocked = if id.is_multiple_of(41) {
        Value::Null
    } else {
        let day = (id * 7) % 1_900;
        Value::Utf8(format!(
            "{}-{:02}-{:02}",
            2019 + day / 365,
            1 + (day % 365) / 31,
            1 + (day % 365) % 28
        ))
    };
    let aisle = if id.is_multiple_of(37) {
        Value::Null
    } else {
        Value::Utf8(AISLES[usize::try_from(id % 3).expect("small")].to_owned())
    };
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), owner, amount, shelf, aisle, stocked],
        1,
        false,
    )
}

fn fixture(key_type: DataType) -> Fixture {
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "owner", key_type, true),
            Column::new(
                3,
                "amount",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                true,
            ),
            Column::new(4, "shelf", DataType::Utf8, true),
            Column::new(5, "aisle", DataType::Utf8, true),
            Column::new(6, "stocked", DataType::Date32, true),
        ],
    )
    .expect("schema");
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).expect("table");
    let rows = 560_000_u64;
    let mut start = 0;
    while start < rows {
        let end = (start + 40_000).min(rows);
        table
            .bulk_ingest_snapshot((start..end).map(|id| stock_row(id, key_type)).collect())
            .expect("ingest");
        start = end;
    }
    let entry = TableEntry::new(
        TableId::new(1),
        "stock",
        schema,
        TableStatistics::with_row_count(rows),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "test", [entry]).expect("database")
    ])
    .expect("catalog");
    Fixture {
        _directory: directory,
        table,
        catalog,
    }
}

fn run(fixture: &Fixture, sql: &str) -> Vec<Vec<Value>> {
    let snapshot = fixture.table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&fixture.catalog, Some("test"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    // A modest ceiling keeps the scatter windows small, so the stream
    // crosses many of them and every transition happens between two.
    let mut execution =
        Execution::start(plan, &provider, 96 << 20, Collation::default()).expect("execution");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("batch") {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| column.value(row).expect("value").clone())
                    .collect::<Vec<_>>(),
            );
        }
    }
    rows
}

const LANES: &str = "COUNT(*), SUM(amount), AVG(amount), MIN(amount), MAX(amount)";

fn assert_same(fixture: &Fixture, direct: &str, general: &str) {
    let left = run(fixture, direct);
    let right = run(fixture, general);
    assert!(!left.is_empty(), "{direct} returned no groups");
    assert_eq!(left.len(), right.len(), "{direct}");
    for (left, right) in left.iter().zip(&right) {
        assert_eq!(left, right, "{direct}");
    }
}

fn integer_range_matches_general(key_type: DataType) {
    let fixture = fixture(key_type);
    for filter in [
        // Every transition: narrow, re-based, sparse, revisited.
        "",
        // Only the range fold: finished straight from its slots.
        "WHERE id < 400000",
        // Selected rows rather than spans.
        "WHERE id % 3 <> 1",
    ] {
        assert_same(
            &fixture,
            &format!("SELECT owner AS k, {LANES} FROM stock {filter} GROUP BY k ORDER BY k"),
            &format!("SELECT owner + 0 AS k, {LANES} FROM stock {filter} GROUP BY k ORDER BY k"),
        );
    }
    // The all-NULL group keeps NULL totals and its row count.
    let key = match key_type {
        DataType::UInt64 => "2000000017",
        _ => "17",
    };
    let rows = run(
        &fixture,
        &format!(
            "SELECT owner, {LANES} FROM stock WHERE id < 400000 GROUP BY owner \
             HAVING owner = {key}"
        ),
    );
    assert_eq!(rows.len(), 1);
    assert!(
        matches!(rows[0][1], Value::Int64(count) if count > 0)
            || matches!(rows[0][1], Value::UInt64(count) if count > 0)
    );
    assert!(
        rows[0][2..]
            .iter()
            .all(|value| matches!(value, Value::Null))
    );
}

#[test]
fn signed_integer_range_fold_matches_general() {
    integer_range_matches_general(DataType::Int64);
}

#[test]
fn unsigned_integer_range_fold_matches_general() {
    integer_range_matches_general(DataType::UInt64);
}

#[test]
fn dense_text_column_folds_match_general() {
    let fixture = fixture(DataType::Int64);
    let unsigned = self::fixture(DataType::UInt64);
    for filter in ["", "WHERE id % 3 <> 1"] {
        assert_same(
            &fixture,
            &format!("SELECT shelf AS k, {LANES} FROM stock {filter} GROUP BY k ORDER BY k"),
            &format!(
                "SELECT CONCAT(shelf, '') AS k, {LANES} FROM stock {filter} GROUP BY k ORDER BY k"
            ),
        );
        assert_same(
            &fixture,
            &format!(
                "SELECT shelf AS a, aisle AS b, {LANES} FROM stock {filter} \
                 GROUP BY a, b ORDER BY a, b"
            ),
            &format!(
                "SELECT CONCAT(shelf, '') AS a, CONCAT(aisle, '') AS b, {LANES} FROM stock \
                 {filter} GROUP BY a, b ORDER BY a, b"
            ),
        );
        // A lane that is not packed beside the packed ones: distinct keys
        // that fit a bitmap, and the sparse stretch that does not.
        for key_type in [DataType::Int64, DataType::UInt64] {
            let fixture = if key_type == DataType::Int64 {
                &fixture
            } else {
                &unsigned
            };
            assert_same(
                fixture,
                &format!(
                    "SELECT shelf AS k, {LANES}, COUNT(DISTINCT owner) FROM stock {filter} \
                     GROUP BY k ORDER BY k"
                ),
                &format!(
                    "SELECT CONCAT(shelf, '') AS k, {LANES}, COUNT(DISTINCT owner) FROM stock \
                     {filter} GROUP BY k ORDER BY k"
                ),
            );
            assert_same(
                fixture,
                &format!(
                    "SELECT shelf AS k, COUNT(DISTINCT owner) FROM stock \
                     WHERE id < 400000 {} GROUP BY k ORDER BY k",
                    filter.replace("WHERE", "AND")
                ),
                &format!(
                    "SELECT CONCAT(shelf, '') AS k, COUNT(DISTINCT owner) FROM stock \
                     WHERE id < 400000 {} GROUP BY k ORDER BY k",
                    filter.replace("WHERE", "AND")
                ),
            );
        }
    }
}

#[test]
fn date_part_folds_match_general() {
    let fixture = fixture(DataType::Int64);
    for filter in ["", "WHERE id % 3 <> 1"] {
        assert_same(
            &fixture,
            &format!(
                "SELECT YEAR(stocked) AS y, MONTH(stocked) AS m, {LANES} FROM stock {filter} \
                 GROUP BY y, m ORDER BY y, m"
            ),
            &format!(
                "SELECT YEAR(stocked) + 0 AS y, MONTH(stocked) + 0 AS m, {LANES} FROM stock \
                 {filter} GROUP BY y, m ORDER BY y, m"
            ),
        );
        assert_same(
            &fixture,
            &format!(
                "SELECT YEAR(stocked) AS y, {LANES} FROM stock {filter} GROUP BY y ORDER BY y"
            ),
            &format!(
                "SELECT YEAR(stocked) + 0 AS y, {LANES} FROM stock {filter} GROUP BY y ORDER BY y"
            ),
        );
        assert_same(
            &fixture,
            &format!("SELECT DAY(stocked) AS d, {LANES} FROM stock {filter} GROUP BY d ORDER BY d"),
            &format!(
                "SELECT DAY(stocked) + 0 AS d, {LANES} FROM stock {filter} GROUP BY d ORDER BY d"
            ),
        );
    }
}

/// Rows in the memtable leave the scan unsettled, so no settled memo keeps
/// the answer as rows, and a range fold over every group serves them as
/// columns - its decimal totals as units - to the rounding and the top-k
/// above it.
#[test]
fn range_fold_groups_served_as_columns_match_general() {
    for key_type in [DataType::Int64, DataType::UInt64] {
        let mut fixture = fixture(key_type);
        fixture
            .table
            .ingest(
                (560_000..566_000)
                    .map(|id| stock_row(id, key_type))
                    .collect(),
            )
            .expect("memtable rows");
        for filter in [
            "WHERE id < 400000 OR id >= 560000",
            "WHERE (id < 400000 OR id >= 560000) AND id % 3 <> 1",
        ] {
            assert_same(
                &fixture,
                &format!("SELECT owner AS k, {LANES} FROM stock {filter} GROUP BY k ORDER BY k"),
                &format!(
                    "SELECT owner + 0 AS k, {LANES} FROM stock {filter} GROUP BY k ORDER BY k"
                ),
            );
            assert_same(
                &fixture,
                &format!(
                    "SELECT owner AS k, COUNT(*) AS n, ROUND(SUM(amount), 1) AS s FROM stock \
                     {filter} GROUP BY k ORDER BY s DESC, k LIMIT 10"
                ),
                &format!(
                    "SELECT owner + 0 AS k, COUNT(*) AS n, ROUND(SUM(amount), 1) AS s FROM stock \
                     {filter} GROUP BY k ORDER BY s DESC, k LIMIT 10"
                ),
            );
            assert_same(
                &fixture,
                &format!(
                    "SELECT owner AS k, SUM(amount) AS s FROM stock {filter} GROUP BY k \
                     ORDER BY s, k LIMIT 25"
                ),
                &format!(
                    "SELECT owner + 0 AS k, SUM(amount) AS s FROM stock {filter} GROUP BY k \
                     ORDER BY s, k LIMIT 25"
                ),
            );
        }
    }
}

/// A range fold reads a window's key bounds ahead of folding it only while
/// the range is unknown or the keys leave it: over a stretch whose keys all
/// lie in the first window's range, everything later is folded against
/// that range - a window at a time or, where the scan folds in place, a
/// round at a time - each key checked as its slot is computed.
#[test]
fn range_fold_reads_key_bounds_only_for_the_first_window_of_a_stable_range() {
    let fixture = fixture(DataType::Int64);
    let _ = pintail_exec::take_exec_counters();
    let rows = run(
        &fixture,
        &format!("SELECT owner AS k, {LANES} FROM stock WHERE id < 200000 GROUP BY k ORDER BY k"),
    );
    let counters = pintail_exec::take_exec_counters();
    // 600 keys and the NULL group.
    assert_eq!(rows.len(), 601);
    assert_eq!(counters.range_windows_bounded, 1, "{counters:?}");
    assert!(
        counters.range_windows_in_range + counters.fused_rounds >= 1,
        "{counters:?}"
    );
    // Keys that widen the range are met by reading the bounds again.
    let _ = run(
        &fixture,
        &format!("SELECT owner AS k, {LANES} FROM stock WHERE id < 400000 GROUP BY k ORDER BY k"),
    );
    let widened = pintail_exec::take_exec_counters();
    assert!(widened.range_windows_bounded >= 2, "{widened:?}");
    assert!(
        widened.range_windows_in_range + widened.fused_rounds >= 1,
        "{widened:?}"
    );
}
