//! A profiled execution reports every plan node once, in plan order, with
//! the rows it produced and the time it took, and `EXPLAIN ANALYZE` prints
//! that profile after the plan. An unprofiled execution reports nothing.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    explain_analyze_statement,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 20_000;

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new() -> Self {
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "grp", DataType::Int64, false),
                Column::new(3, "amount", DataType::Int64, false),
            ],
        )
        .expect("schema");
        let directory = tempfile::tempdir().expect("directory");
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("table");
        let rows = (1..=ROWS)
            .map(|id| {
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                    vec![
                        Value::UInt64(id),
                        Value::Int64(i64::try_from(id % 50).expect("small")),
                        Value::Int64(i64::try_from(id % 7).expect("small")),
                    ],
                    id,
                    false,
                )
            })
            .collect();
        table.bulk_ingest_snapshot(rows).expect("ingest");
        let entry = TableEntry::new(
            TableId::new(1),
            "events",
            schema,
            TableStatistics::with_row_count(ROWS),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database");
        Self {
            _directory: directory,
            table,
            catalog: CatalogSnapshot::new([database]).expect("catalog"),
        }
    }
}

const SQL: &str = "SELECT grp, COUNT(*), SUM(amount) FROM events WHERE id >= 1 GROUP BY grp \
                   ORDER BY grp LIMIT 10";

#[test]
fn a_profiled_execution_reports_every_plan_node_with_its_rows_and_time() {
    let fixture = Fixture::new();
    let snapshot = fixture.table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&fixture.catalog, Some("app"))
        .bind(&parse_statement(SQL).expect("parse"))
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");

    // Profiled first: a settled aggregate memoizes its answer, and a later
    // run would report the memo's rows against a scan that never ran.
    let mut profiled = Execution::start_profiled(
        physical.clone(),
        &provider,
        64 << 20,
        None,
        Collation::default(),
    )
    .expect("start");
    let mut rows = 0;
    while let Some(batch) = profiled.next_batch().expect("batch") {
        rows += batch.visible_row_count();
    }
    let profile = profiled.profile().expect("a profile");
    let labels: Vec<&str> = profile
        .operators
        .iter()
        .map(|node| node.label.as_str())
        .collect();
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("Scan app.events")),
        "the scan is a plan node: {labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("HashAggregate")),
        "the aggregate is a plan node: {labels:?}"
    );
    let root = &profile.operators[0];
    assert_eq!(root.depth, 0);
    assert_eq!(root.rows, u64::try_from(rows).expect("small"));
    assert!(root.batches >= 1);
    assert!(
        profile
            .operators
            .iter()
            .all(|node| node.inclusive <= root.inclusive),
        "no node takes longer than the root that pulls it"
    );
    assert!(profile.total >= root.inclusive);
    let scan = profile
        .operators
        .iter()
        .find(|node| node.label.starts_with("Scan"))
        .expect("scan");
    assert_eq!(scan.rows, ROWS, "the filter excludes nothing");
    let rendered = profile.render();
    assert!(rendered.starts_with("Profile total="));
    assert!(rendered.contains("self="), "{rendered}");

    let explained = explain_analyze_statement(
        &parse_statement(&format!("EXPLAIN ANALYZE {SQL}")).expect("parse"),
        &fixture.catalog,
        Some("app"),
        &provider,
        64 << 20,
    )
    .expect("explain analyze");
    assert!(explained.contains("Spill files="), "{explained}");
    assert!(explained.contains("Profile total="), "{explained}");
    assert!(explained.contains("HashAggregate"), "{explained}");

    let mut plain =
        Execution::start(physical, &provider, 64 << 20, Collation::default()).expect("start");
    while plain.next_batch().expect("batch").is_some() {}
    assert!(
        plain.profile().is_none(),
        "an unprofiled execution records nothing"
    );
}
