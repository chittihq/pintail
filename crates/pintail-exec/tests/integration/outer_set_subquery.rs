//! Correlated scalar aggregate subqueries answered a batch of outer rows
//! at a time give the answers the per-row path gives.
//!
//! Every expectation is computed here from the fixture's rows, not by a
//! second run of the engine: the value an aggregate has over no rows (0
//! for `COUNT`, NULL for `SUM` and `AVG`), a NULL outer value, outer rows
//! that repeat a value, a nested `IN` correlated to another outer column,
//! `COUNT(DISTINCT)`, a LEFT JOIN inside the subquery, the subquery used
//! by HAVING and ORDER BY as well as the select list, and the shapes that
//! must stay on the per-row path.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    dependent_set_executions, dependent_subquery_executions,
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

const DATABASE: u64 = 11;
const TABLES: [u64; 3] = [111, 112, 113];

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

/// The rows of `sql`, the per-row inner executions and the set executions
/// the dependent path ran for it.
fn run(fixture: &Fixture, sql: &str) -> (Vec<Vec<String>>, u64, u64) {
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
    let sets_before = dependent_set_executions();
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
    (
        rows,
        dependent_subquery_executions() - before,
        dependent_set_executions() - sets_before,
    )
}

const PARCELS: u64 = 300;

/// Scan `k`'s columns: its parcel, `ok` (NULL for every fifth) and zone.
fn scan(k: u64) -> (u64, Option<u64>, u64) {
    (
        k % SCANNED + 1,
        (!k.is_multiple_of(5)).then_some(k % 3),
        k % 4 + 1,
    )
}

fn scans_of(parcel: u64) -> Vec<(Option<u64>, u64)> {
    (1..=SCANS)
        .map(scan)
        .filter(|(owner, ..)| *owner == parcel)
        .map(|(_, ok, zone)| (ok, zone))
        .collect()
}

fn weight(parcel: u64) -> u64 {
    parcel * 31 % 101
}

fn nullable(value: Option<u64>) -> String {
    value.map_or_else(|| "NULL".to_owned(), |value| value.to_string())
}

#[test]
fn aggregates_over_a_join_answer_every_row_in_one_execution() {
    let fixture = fixture(PARCELS);
    let (rows, per_row, sets) = run(
        &fixture,
        "SELECT p.id, \
           (SELECT SUM(s.ok = 1) FROM scans s INNER JOIN zones z ON z.id = s.zone_id \
             WHERE s.parcel_id = p.id AND z.open = 1) AS ok_open, \
           (SELECT COUNT(*) FROM scans s INNER JOIN zones z ON z.id = s.zone_id \
             WHERE s.parcel_id = p.id) AS scans, \
           (SELECT COUNT(DISTINCT s.ok) FROM scans s LEFT JOIN zones z \
               ON z.id = s.zone_id AND z.open = 1 \
             WHERE s.parcel_id = p.id AND z.id IS NULL) AS closed_oks, \
           (SELECT MAX(s.id) FROM scans s WHERE s.parcel_id = p.id \
               AND s.zone_id IN (SELECT z.id FROM zones z WHERE z.open = p.weight % 2)) AS latest \
         FROM parcels p ORDER BY p.id",
    );
    assert_eq!(rows.len() as u64, PARCELS);
    for row in &rows {
        let parcel: u64 = row[0].parse().expect("id");
        let scans = scans_of(parcel);
        let open = scans
            .iter()
            .filter(|(_, zone)| zone % 2 == 1)
            .collect::<Vec<_>>();
        // `NULL = 1` is NULL and SUM skips it: only scans with an `ok` count.
        let ok_open = open
            .iter()
            .any(|(ok, _)| ok.is_some())
            .then(|| open.iter().filter(|(ok, _)| *ok == Some(1)).count() as u64);
        let mut closed = scans
            .iter()
            .filter(|(ok, zone)| zone % 2 == 0 && ok.is_some())
            .map(|(ok, _)| *ok)
            .collect::<Vec<_>>();
        closed.sort_unstable();
        closed.dedup();
        let wanted = weight(parcel) % 2;
        let latest = (1..=SCANS)
            .filter(|k| scan(*k).0 == parcel && scan(*k).2 % 2 == wanted)
            .max();
        assert_eq!(
            row[1..],
            [
                nullable(ok_open),
                scans.len().to_string(),
                closed.len().to_string(),
                nullable(latest),
            ],
            "parcel {parcel}"
        );
    }
    // Four subqueries: each asks once what it answers over no rows, and
    // answers the batch in one execution.
    assert_eq!(per_row, 4, "one empty-input probe per subquery");
    assert_eq!(sets, 4, "one set execution per subquery");
}

#[test]
fn null_and_repeated_outer_values_answer_as_the_per_row_path_does() {
    let fixture = fixture(PARCELS);
    // `label` is NULL for every seventeenth parcel and repeats otherwise;
    // the subquery counts the parcels sharing the outer row's label.
    let (rows, _, sets) = run(
        &fixture,
        "SELECT p.id, p.label, \
           (SELECT COUNT(*) FROM parcels q WHERE q.label = p.label) AS same, \
           (SELECT SUM(q.weight) FROM parcels q WHERE q.label = p.label AND q.id < p.id) AS before \
         FROM parcels p ORDER BY p.id",
    );
    let label = |id: u64| (!id.is_multiple_of(17)).then(|| id * 7 % 13);
    for row in &rows {
        let parcel: u64 = row[0].parse().expect("id");
        let same = label(parcel).map_or(0, |own| {
            (1..=PARCELS).filter(|id| label(*id) == Some(own)).count()
        });
        let earlier = (1..parcel)
            .filter(|id| label(parcel).is_some() && label(*id) == label(parcel))
            .map(weight)
            .collect::<Vec<_>>();
        let before = (!earlier.is_empty()).then(|| earlier.iter().sum::<u64>());
        assert_eq!(
            row[2..],
            [same.to_string(), nullable(before)],
            "parcel {parcel}"
        );
    }
    assert!(sets >= 1, "the single-table aggregates take the set form");
}

#[test]
fn having_and_order_by_share_the_select_list_answers() {
    let fixture = fixture(PARCELS);
    let select = "SELECT p.id, \
           (SELECT COUNT(*) FROM scans s INNER JOIN zones z ON z.id = s.zone_id \
             WHERE s.parcel_id = p.id AND z.open = 1) AS open_scans \
         FROM parcels p";
    let (rows, per_row, sets) = run(
        &fixture,
        &format!("{select} HAVING open_scans >= 2 ORDER BY open_scans DESC, p.id LIMIT 0, 500"),
    );
    let mut expected = (1..=PARCELS)
        .map(|parcel| {
            let open = scans_of(parcel)
                .iter()
                .filter(|(_, zone)| zone % 2 == 1)
                .count();
            (parcel, open)
        })
        .filter(|(_, open)| *open >= 2)
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    assert_eq!(
        rows,
        expected
            .iter()
            .map(|(parcel, open)| vec![parcel.to_string(), open.to_string()])
            .collect::<Vec<_>>()
    );
    assert_eq!(per_row, 1, "the value over no rows is taken once");
    assert_eq!(
        sets, 1,
        "HAVING answers every row; ORDER BY and the select list read its answers"
    );
}

#[test]
fn shapes_that_are_not_one_row_per_outer_row_stay_per_row() {
    let fixture = fixture(40);
    // A lookup, not an aggregate: more than one scan for parcel 3 is the
    // cardinality error, raised as it always was.
    let database_id = DatabaseId::new(DATABASE);
    let provider = SnapshotScanProvider::new([
        (database_id, TableId::new(TABLES[0]), &fixture.snapshots[0]),
        (database_id, TableId::new(TABLES[1]), &fixture.snapshots[1]),
        (database_id, TableId::new(TABLES[2]), &fixture.snapshots[2]),
    ])
    .expect("provider");
    let sql = "SELECT p.id, (SELECT s.zone_id + z.open FROM scans s INNER JOIN zones z \
                 ON z.id = s.zone_id WHERE s.parcel_id = p.id) AS zone FROM parcels p";
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&fixture.catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let error = match Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
    {
        Err(error) => error,
        Ok(mut execution) => loop {
            match execution.next_batch() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("three scans for one parcel must be an error"),
                Err(error) => break error,
            }
        },
    };
    assert!(
        error.to_string().contains("scalar subquery produced"),
        "unexpected error: {error}"
    );
    // A grouped subquery and one with its own LIMIT are not rewritten.
    let (rows, _, sets) = run(
        &fixture,
        "SELECT p.id, (SELECT COUNT(*) FROM scans s WHERE s.parcel_id = p.id LIMIT 1) AS n \
         FROM parcels p ORDER BY p.id",
    );
    assert_eq!(sets, 0, "a subquery with its own LIMIT stays per-row");
    for row in &rows {
        let parcel: u64 = row[0].parse().expect("id");
        assert_eq!(row[1], scans_of(parcel).len().to_string());
    }
}

#[test]
fn the_first_row_of_an_ordering_over_a_join_answers_every_row_at_once() {
    let fixture = fixture(PARCELS);
    let (rows, per_row, sets) = run(
        &fixture,
        "SELECT p.id, \
           (SELECT z.open * 1000 + s.id FROM scans s INNER JOIN zones z ON z.id = s.zone_id \
             WHERE s.parcel_id = p.id AND s.ok IS NOT NULL ORDER BY s.id DESC LIMIT 1) AS latest \
         FROM parcels p ORDER BY p.id",
    );
    assert_eq!(rows.len() as u64, PARCELS);
    for row in &rows {
        let parcel: u64 = row[0].parse().expect("id");
        let latest = (1..=SCANS)
            .filter(|k| scan(*k).0 == parcel && scan(*k).1.is_some())
            .max()
            .map(|k| scan(k).2 % 2 * 1000 + k);
        assert_eq!(row[1], nullable(latest), "parcel {parcel}");
    }
    assert_eq!(per_row, 1, "only the value over no rows is asked per row");
    assert_eq!(sets, 1);
}

#[test]
fn a_subquery_with_nothing_to_join_the_outer_rows_on_stays_per_row() {
    let fixture = fixture(60);
    // Correlated only through the nested IN: the outer rows restrict no
    // inner table directly.
    let (rows, _, sets) = run(
        &fixture,
        "SELECT p.id, (SELECT COUNT(*) FROM scans s WHERE s.zone_id IN \
           (SELECT z.id FROM zones z WHERE z.open = p.weight % 2)) AS n \
         FROM parcels p ORDER BY p.id",
    );
    assert_eq!(sets, 0);
    for row in &rows {
        let parcel: u64 = row[0].parse().expect("id");
        let wanted = weight(parcel) % 2;
        let expected = (1..=SCANS).filter(|k| scan(*k).2 % 2 == wanted).count();
        assert_eq!(row[1], expected.to_string(), "parcel {parcel}");
    }
}
