//! Query shapes that were reported failing inside the engine, each over an
//! invented schema: every one has to bind, plan and run.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const VISITS: u64 = 12;
const PLACES: u64 = 4;

fn visits_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "kind", DataType::Utf8, false),
            Column::new(3, "place_id", DataType::UInt64, true),
            Column::new(4, "other_id", DataType::Int64, true),
            Column::new(5, "open", DataType::Boolean, false),
            Column::new(6, "zone", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn places_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "zone", DataType::Int64, false),
            Column::new(3, "label", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

/// How many rows `sql` answers, or why it did not.
#[allow(clippy::too_many_lines)] // two tables built and registered in one place
fn rows(sql: &str) -> Result<usize, String> {
    let directory = tempfile::tempdir().expect("directory");
    let mut visits = TableStore::open(
        directory.path().join("visits"),
        visits_schema(),
        StoreOptions::default(),
    )
    .expect("table");
    visits
        .bulk_ingest_snapshot(
            (0..VISITS)
                .map(|id| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            Value::Utf8(if id % 3 == 0 { "walk" } else { "ride" }.to_owned()),
                            if id % 5 == 0 {
                                Value::Null
                            } else {
                                Value::UInt64(id % PLACES)
                            },
                            Value::Int64(i64::try_from(id % PLACES).expect("small")),
                            Value::Boolean(id % 2 == 0),
                            Value::Int64(i64::try_from(id % 3).expect("small")),
                        ],
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let mut places = TableStore::open(
        directory.path().join("places"),
        places_schema(),
        StoreOptions::default(),
    )
    .expect("table");
    places
        .bulk_ingest_snapshot(
            (0..PLACES)
                .map(|id| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            Value::Int64(i64::try_from(id % 3).expect("small")),
                            Value::Utf8(format!("place-{id}")),
                        ],
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let (visits_snapshot, places_snapshot) = (visits.snapshot(), places.snapshot());
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        DatabaseId::new(1),
        "app",
        [
            TableEntry::new(
                TableId::new(1),
                "visits",
                visits_schema(),
                TableStatistics::with_row_count(VISITS),
            )
            .expect("entry"),
            TableEntry::new(
                TableId::new(2),
                "places",
                places_schema(),
                TableStatistics::with_row_count(PLACES),
            )
            .expect("entry"),
        ],
    )
    .expect("database")])
    .expect("catalog");
    let provider = SnapshotScanProvider::new([
        (DatabaseId::new(1), TableId::new(1), &visits_snapshot),
        (DatabaseId::new(1), TableId::new(2), &places_snapshot),
    ])
    .expect("provider");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).map_err(|error| format!("parse: {error}"))?)
        .map_err(|error| format!("bind: {error}"))?;
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .map_err(|error| format!("plan: {error}"))?;
    let mut execution = Execution::start(plan, &provider, 64 << 20, Collation::default())
        .map_err(|error| format!("start: {error}"))?;
    let mut count = 0;
    while let Some(batch) = execution
        .next_batch()
        .map_err(|error| format!("run: {error}"))?
    {
        count += batch.selection().selected_rows().count();
    }
    Ok(count)
}

#[test]
fn reported_shapes_run() {
    let cases = [
        // A join keyed by a CASE over the left side.
        "SELECT COUNT(DISTINCT v.id) AS c FROM visits v \
         LEFT JOIN places p ON p.id = CASE WHEN v.kind = 'walk' THEN v.place_id \
         ELSE v.other_id END",
        "SELECT COUNT(DISTINCT v.id) AS c FROM visits v \
         LEFT JOIN places p ON p.id = CASE WHEN v.kind = 'walk' THEN v.place_id END",
        "SELECT COUNT(DISTINCT v.id) AS c FROM visits v \
         LEFT JOIN places p ON p.id = CASE WHEN v.kind = 'walk' THEN v.place_id \
         WHEN v.kind = 'ride' THEN v.other_id ELSE NULL END \
         LEFT JOIN places q ON q.id = CASE WHEN v.kind = 'walk' THEN NULL ELSE v.other_id END",
        "SELECT COUNT(DISTINCT v.id) AS c FROM visits v \
         LEFT JOIN places p ON p.label = CASE WHEN v.kind = 'walk' THEN v.place_id \
         ELSE v.kind END",
        "SELECT COUNT(DISTINCT v.id) AS c FROM visits v \
         LEFT JOIN places p ON p.id = CASE WHEN v.kind = 'walk' THEN NULL END",
        // A grouped report whose select list aggregates a column of the
        // grouped table beside correlated subqueries.
        "SELECT v.zone, MIN(v.open), \
         (SELECT COUNT(DISTINCT p.id) FROM places p WHERE p.zone = v.zone) \
         FROM visits v GROUP BY v.zone",
        "SELECT v.zone, MIN(v.open) AS open, MIN(p.label) AS label, \
         (SELECT COUNT(*) FROM places q WHERE q.zone = v.zone) AS near, \
         (SELECT COUNT(*) FROM visits w WHERE w.zone = v.zone AND w.open = 1) AS open_visits \
         FROM visits v LEFT JOIN places p ON p.id = v.place_id \
         GROUP BY v.zone ORDER BY open_visits DESC, v.zone",
        // EXISTS inside an aggregate's argument.
        "SELECT v.kind, COUNT(*) AS n, \
         SUM(EXISTS(SELECT 1 FROM places p WHERE p.id = v.place_id AND p.zone = 1)) AS near \
         FROM visits v GROUP BY v.kind",
        "SELECT COUNT(*) AS n, \
         COUNT(CASE WHEN EXISTS(SELECT 1 FROM places p WHERE p.id = v.place_id) THEN 1 END) \
         FROM visits v",
        "SELECT v.kind, COUNT(DISTINCT CASE WHEN v.place_id IN (SELECT p.id FROM places p \
         WHERE p.zone = 1) THEN v.id END) FROM visits v GROUP BY v.kind",
    ];
    let mut failed = Vec::new();
    for sql in cases {
        match rows(sql) {
            Ok(count) => eprintln!("OK {count} rows: {sql}"),
            Err(error) => {
                eprintln!("FAILED {error}: {sql}");
                failed.push(format!("{error}: {sql}"));
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
