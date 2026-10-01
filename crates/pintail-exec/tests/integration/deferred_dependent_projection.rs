//! Select-list subqueries under a `LIMIT` run only for the rows it keeps.
//!
//! A correlated subquery in the select list is answered per row. Under
//! `ORDER BY .. LIMIT` the rows that survive are decided by the sort keys
//! alone, so a subquery column that is not a sort key is evaluated after
//! the limit, for the surviving rows only. These tests pin both halves of
//! that: the answer is the page the undeferred evaluation gives - same
//! rows, same order through ties, same values - and the inner query runs
//! once per surviving row instead of once per input row.
//!
//! The reference for which rows survive is the same `ORDER BY .. LIMIT`
//! with no subquery in the select list (the ordinary sort), and the
//! reference for the values is the unlimited query, which evaluates every
//! row before any sort.

use std::collections::HashMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    dependent_subquery_executions,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn parcels_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, true)
                .with_collation(Some("utf8mb4_0900_ai_ci".to_owned())),
            Column::new(3, "weight", DataType::Int64, false),
        ],
    )
    .expect("parcels schema")
}

fn scans_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "parcel_id", DataType::UInt64, false),
            Column::new(3, "ok", DataType::Int64, true),
            Column::new(4, "zone_id", DataType::UInt64, false),
        ],
    )
    .expect("scans schema")
}

fn zones_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "open", DataType::Int64, false),
        ],
    )
    .expect("zones schema")
}

fn stored(id: u64, values: Vec<Value>) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        values,
        id,
        false,
    )
}

/// Parcels with scans: every scan `k` belongs to parcel `k % SCANNED + 1`.
const SCANNED: u64 = 200;
const SCANS: u64 = 600;

/// `parcels` rows of labels that repeat heavily (and are NULL for every
/// seventeenth), so every page boundary falls inside a tie group; 600 scans
/// over the first 200 parcels; four zones, the odd ones open.
struct Fixture {
    _directories: [tempfile::TempDir; 3],
    snapshots: [pintail_store::TableSnapshot; 3],
    catalog: CatalogSnapshot,
}

const DATABASE: u64 = 9;
const TABLES: [u64; 3] = [91, 92, 93];

fn fixture(parcels: u64) -> Fixture {
    let directories = [(); 3].map(|()| tempfile::tempdir().expect("table dir"));
    let parcel_rows = (1..=parcels)
        .map(|id| {
            let label = if id % 17 == 0 {
                Value::Null
            } else {
                Value::Utf8(format!("L{:02}", id * 7 % 13))
            };
            stored(
                id,
                vec![
                    Value::UInt64(id),
                    label,
                    Value::Int64(i64::try_from(id * 31 % 101).expect("weight")),
                ],
            )
        })
        .collect::<Vec<_>>();
    let scan_rows = (1..=SCANS)
        .map(|k| {
            let ok = match k % 5 {
                0 => Value::Null,
                _ => Value::Int64(i64::try_from(k % 3).expect("ok")),
            };
            stored(
                k,
                vec![
                    Value::UInt64(k),
                    Value::UInt64(k % SCANNED + 1),
                    ok,
                    Value::UInt64(k % 4 + 1),
                ],
            )
        })
        .collect::<Vec<_>>();
    let zone_rows = (1..=4_u64)
        .map(|id| {
            stored(
                id,
                vec![
                    Value::UInt64(id),
                    Value::Int64(i64::try_from(id % 2).expect("open")),
                ],
            )
        })
        .collect::<Vec<_>>();
    let schemas = [parcels_schema(), scans_schema(), zones_schema()];
    let names = ["parcels", "scans", "zones"];
    let mut snapshots = Vec::new();
    let mut entries = Vec::new();
    for (index, rows) in [parcel_rows, scan_rows, zone_rows].into_iter().enumerate() {
        let count = rows.len() as u64;
        let mut store = TableStore::open(
            directories[index].path(),
            schemas[index].clone(),
            StoreOptions::default(),
        )
        .expect("open table");
        store.bulk_ingest_snapshot(rows).expect("snapshot rows");
        snapshots.push(store.snapshot());
        entries.push(
            TableEntry::new(
                TableId::new(TABLES[index]),
                names[index],
                schemas[index].clone(),
                TableStatistics::with_row_count(count),
            )
            .expect("table entry"),
        );
    }
    let database = DatabaseEntry::new(DatabaseId::new(DATABASE), "app", entries).expect("database");
    Fixture {
        _directories: directories,
        snapshots: snapshots.try_into().ok().expect("three snapshots"),
        catalog: CatalogSnapshot::new([database]).expect("catalog"),
    }
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(value) => u8::from(*value).to_string(),
        Value::Int64(value) => value.to_string(),
        Value::UInt64(value) => value.to_string(),
        Value::Utf8(value) => value.clone(),
        other => format!("{other:?}"),
    }
}

/// The rows of `sql` and the inner queries the dependent path executed.
fn run(fixture: &Fixture, sql: &str) -> (Vec<Vec<String>>, u64) {
    let database_id = DatabaseId::new(DATABASE);
    let provider = SnapshotScanProvider::new([
        (database_id, TableId::new(TABLES[0]), &fixture.snapshots[0]),
        (database_id, TableId::new(TABLES[1]), &fixture.snapshots[1]),
        (database_id, TableId::new(TABLES[2]), &fixture.snapshots[2]),
    ])
    .expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&fixture.catalog, Some("app"))
        .bind(&statement)
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let before = dependent_subquery_executions();
    let mut execution =
        Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
            .unwrap_or_else(|error| panic!("start {sql}: {error}"));
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("run {sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            rows.push(
                (0..batch.columns().len())
                    .map(|column| {
                        render(
                            batch
                                .column(column)
                                .and_then(|values| values.value(row))
                                .expect("selected value"),
                        )
                    })
                    .collect(),
            );
        }
    }
    (rows, dependent_subquery_executions() - before)
}

/// Scans of the parcel in an open zone that are ok: NULL without one.
///
/// `LIMIT 1` changes no answer - an ungrouped aggregate is one row - and
/// keeps both subqueries on the per-row path, whose executions these tests
/// count; without it they are answered a batch at a time.
const OK_SCANS: &str = "(SELECT SUM(s.ok = 1) FROM scans s INNER JOIN zones z ON z.id = s.zone_id \
                        WHERE s.parcel_id = p.id AND z.open = 1 LIMIT 1)";
/// Every scan of the parcel.
const ALL_SCANS: &str = "(SELECT COUNT(*) FROM scans s2 INNER JOIN zones z2 ON z2.id = s2.zone_id \
                         WHERE s2.parcel_id = p.id LIMIT 1)";

const PARCELS: u64 = 300;

#[test]
fn a_page_runs_its_subqueries_for_its_own_rows_only() {
    let fixture = fixture(PARCELS);
    let select =
        format!("SELECT p.id, p.label, {OK_SCANS} AS okScans, {ALL_SCANS} AS scans FROM parcels p");
    let (every, all_executions) = run(&fixture, &format!("{select} ORDER BY p.label"));
    assert_eq!(every.len() as u64, PARCELS);
    assert_eq!(
        all_executions,
        PARCELS * 2,
        "one execution per row per subquery"
    );
    // Parcel 3 holds scans 2, 202 and 402, all in zone 3 (open); only
    // scan 202 has ok = 1 (2 % 3 = 2, 402 % 3 = 0).
    let first = every.iter().find(|row| row[0] == "3").expect("parcel 3");
    assert_eq!(&first[2..], ["1", "3"]);
    // A parcel with no scan at all: SUM over nothing is NULL, COUNT is 0.
    let bare = every
        .iter()
        .find(|row| row[0] == "250")
        .expect("parcel 250");
    assert_eq!(&bare[2..], ["NULL", "0"]);

    for (offset, count) in [
        (0_usize, 20_usize),
        (5, 10),
        (17, 1),
        (290, 20),
        (300, 5),
        (1000, 5),
    ] {
        let (page, executions) = run(
            &fixture,
            &format!("{select} ORDER BY p.label LIMIT {offset}, {count}"),
        );
        let expected = every
            .iter()
            .skip(offset)
            .take(count)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(page, expected, "LIMIT {offset}, {count}");
        assert_eq!(
            executions,
            expected.len() as u64 * 2,
            "LIMIT {offset}, {count}: the subqueries run for the surviving rows only"
        );
    }
    // Descending, NULL labels last, ties still in arrival order.
    let (descending, _) = run(&fixture, &format!("{select} ORDER BY p.label DESC"));
    let (page, executions) = run(
        &fixture,
        &format!("{select} ORDER BY p.label DESC LIMIT 40, 15"),
    );
    assert_eq!(page, descending[40..55]);
    assert_eq!(executions, 30);
}

#[test]
fn the_surviving_rows_are_the_ones_the_plain_sort_keeps() {
    let fixture = fixture(PARCELS);
    for order in ["p.label", "p.label DESC", "p.weight, p.label DESC"] {
        let (plain, _) = run(
            &fixture,
            &format!("SELECT p.id FROM parcels p ORDER BY {order} LIMIT 23, 31"),
        );
        let (page, _) = run(
            &fixture,
            &format!(
                "SELECT p.id, {OK_SCANS} AS okScans FROM parcels p ORDER BY {order} LIMIT 23, 31"
            ),
        );
        let ids = |rows: &[Vec<String>]| rows.iter().map(|row| row[0].clone()).collect::<Vec<_>>();
        assert_eq!(ids(&page), ids(&plain), "ORDER BY {order}");
    }
}

#[test]
fn a_sort_key_outside_the_select_list_is_trimmed_after_the_page() {
    let fixture = fixture(PARCELS);
    let select =
        format!("SELECT p.id, {OK_SCANS} AS okScans FROM parcels p ORDER BY p.weight DESC, p.id");
    let (every, _) = run(&fixture, &select);
    let (page, executions) = run(&fixture, &format!("{select} LIMIT 3, 4"));
    assert_eq!(page, every[3..7]);
    assert!(
        page.iter().all(|row| row.len() == 2),
        "the hidden sort key is not returned"
    );
    assert_eq!(executions, 4);
}

#[test]
fn a_subquery_the_sort_reads_runs_for_every_row_and_the_others_wait() {
    let fixture = fixture(PARCELS);
    let select = format!(
        "SELECT p.id, {OK_SCANS} AS okScans, {ALL_SCANS} AS scans FROM parcels p ORDER BY scans DESC, p.id"
    );
    let (every, _) = run(&fixture, &select);
    let (page, executions) = run(&fixture, &format!("{select} LIMIT 2, 6"));
    assert_eq!(page, every[2..8]);
    assert_eq!(
        executions,
        PARCELS + 6,
        "the key column for every row, the other for the page"
    );
}

#[test]
fn a_limit_with_no_order_stops_reading_and_evaluating() {
    let fixture = fixture(PARCELS);
    let select = format!("SELECT p.id, {OK_SCANS} AS okScans FROM parcels p");
    let (every, _) = run(&fixture, &select);
    let (page, executions) = run(&fixture, &format!("{select} LIMIT 3"));
    assert_eq!(page, every[..3]);
    assert_eq!(executions, 3);
    let (page, executions) = run(&fixture, &format!("{select} LIMIT 4, 3"));
    assert_eq!(page, every[4..7]);
    assert_eq!(executions, 3);
    let (page, executions) = run(&fixture, &format!("{select} LIMIT 0"));
    assert!(page.is_empty());
    assert_eq!(executions, 0);
}

#[test]
fn a_filter_on_the_subquery_still_sees_every_row() {
    let fixture = fixture(PARCELS);
    let select = format!(
        "SELECT p.id, {OK_SCANS} AS okScans, {ALL_SCANS} AS scans FROM parcels p HAVING okScans >= 1 ORDER BY p.label, p.id"
    );
    let (every, _) = run(&fixture, &select);
    assert!(!every.is_empty() && every.len() < 200);
    assert!(every.iter().all(|row| row[2] != "NULL" && row[2] != "0"));
    let (page, _) = run(&fixture, &format!("{select} LIMIT 5, 9"));
    assert_eq!(page, every[5..14]);
}

/// More candidates than the operator holds at once: they are cut back to
/// the page while the input is still being read, and the page must be the
/// one a single sort of everything keeps.
#[test]
fn candidates_cut_back_mid_input_keep_the_page_of_one_full_sort() {
    let parcels = 30_000;
    let fixture = fixture(parcels);
    let (reference, _) = run(
        &fixture,
        &format!("SELECT p.id, {OK_SCANS} AS okScans FROM parcels p WHERE p.id <= {SCANNED}"),
    );
    let ok_scans = reference
        .into_iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect::<HashMap<_, _>>();
    for (order, offset, count) in [
        ("p.label", 5, 10),
        ("p.label DESC", 0, 20),
        ("p.weight, p.label", 2000, 20),
    ] {
        let (plain, _) = run(
            &fixture,
            &format!("SELECT p.id FROM parcels p ORDER BY {order} LIMIT {offset}, {count}"),
        );
        let (page, executions) = run(
            &fixture,
            &format!(
                "SELECT p.id, {OK_SCANS} AS okScans FROM parcels p ORDER BY {order} LIMIT {offset}, {count}"
            ),
        );
        assert_eq!(page.len(), count);
        assert_eq!(executions, count as u64, "ORDER BY {order}");
        for (row, plain) in page.iter().zip(&plain) {
            assert_eq!(
                row[0], plain[0],
                "ORDER BY {order}: same rows in the same order"
            );
            let expected = ok_scans.get(&row[0]).map_or("NULL", String::as_str);
            assert_eq!(row[1], expected, "parcel {}", row[0]);
        }
    }
}
