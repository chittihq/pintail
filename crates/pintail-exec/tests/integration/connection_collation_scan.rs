//! A scan evaluates its pushed predicates itself to choose rows, and must
//! compare text as the Filter above it does: under the collation the plan
//! resolved, which follows the connection's when no column decides it. A
//! server builds its provider before the statement is bound, so the
//! provider cannot know that collation; the execution hands it to every
//! scan it opens instead. These tests record what each scan was given.

use std::sync::{Arc, Mutex};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    BatchStream, ExecError, Execution, LogicalPlanner, Optimizer, PhysicalPlanner, Scan,
    ScanProvider, SnapshotScanProvider,
};
use pintail_sql::{Binder, parse_statement, set_session_default_collation};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const GENERAL_CI: &str = "utf8mb4_general_ci";

/// (id, n, label).
const ROWS: [(u64, i64, &str); 6] = [
    (1, 3, "ann"),
    (2, 5, "ANN"),
    (3, 7, "bob"),
    (4, 5, "ann "),
    (5, 9, "anna"),
    (6, 2, "Ann"),
];

/// What each scan an execution opened was told to compare text under;
/// `None` for a scan opened without one.
type Seen = Arc<Mutex<Vec<Option<Collation>>>>;

/// Opens scans through `inner`, recording the collation each was given,
/// for the per-table providers it hands out too.
struct Recording<P> {
    inner: P,
    seen: Seen,
}

impl<P, T> ScanProvider for Recording<P>
where
    P: std::ops::Deref<Target = T>,
    T: ScanProvider + ?Sized,
{
    fn open_scan(
        &self,
        scan: &Scan,
        memory_limit: usize,
    ) -> Result<Box<dyn BatchStream>, ExecError> {
        self.seen.lock().expect("seen").push(None);
        self.inner.open_scan(scan, memory_limit)
    }

    fn open_scan_collated(
        &self,
        scan: &Scan,
        memory_limit: usize,
        collation: Collation,
    ) -> Result<Box<dyn BatchStream>, ExecError> {
        self.seen.lock().expect("seen").push(Some(collation));
        self.inner.open_scan_collated(scan, memory_limit, collation)
    }

    fn table_provider(
        &self,
        database_id: DatabaseId,
        table_id: TableId,
    ) -> Option<Box<dyn ScanProvider + Send + Sync>> {
        let inner = self.inner.table_provider(database_id, table_id)?;
        Some(Box::new(Recording {
            inner,
            seen: Arc::clone(&self.seen),
        }))
    }
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "n", DataType::Int64, false),
            Column::new(3, "label", DataType::Utf8, false)
                .with_collation(Some("utf8mb4_0900_ai_ci".to_owned())),
        ],
    )
    .expect("schema")
}

fn row(id: u64, n: i64, label: &str) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(n),
            Value::Utf8(label.to_owned()),
        ],
        id,
        false,
    )
}

/// Runs `sql` under the connection collation `connection` over a provider
/// built, as a server builds it, before the statement is bound. Returns the
/// plan's collation, the collations the scans were given, and the rows.
fn run(sql: &str, connection: &'static str) -> (Collation, Vec<Option<Collation>>, usize) {
    let dir = tempfile::tempdir().expect("temporary table");
    let mut store =
        TableStore::open(dir.path(), schema(), StoreOptions::default()).expect("open table");
    store
        .bulk_ingest_snapshot(
            ROWS.iter()
                .map(|(id, n, label)| row(*id, *n, label))
                .collect(),
        )
        .expect("bulk rows");
    let snapshot = store.snapshot();
    let database_id = DatabaseId::new(15);
    let table_id = TableId::new(17);
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [TableEntry::new(
            table_id,
            "items",
            schema(),
            TableStatistics::with_row_count(ROWS.len() as u64),
        )
        .expect("table entry")],
    )
    .expect("database entry");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let seen = Seen::default();
    let snapshots =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    let provider = Recording {
        inner: &snapshots,
        seen: Arc::clone(&seen),
    };

    set_session_default_collation(Some(connection));
    let statement = parse_statement(sql).expect("parse query");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind query");
    set_session_default_collation(None);
    let collation = Collation::from_mysql_name(bound.text_collation).expect("collation");
    let physical =
        PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)), collation)
            .expect("physical plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, collation).expect("start");
    let mut rows = 0;
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
    {
        rows += batch.selection().selected_rows().count();
    }
    let seen = seen.lock().expect("seen").clone();
    (collation, seen, rows)
}

fn assert_every_scan_compares_under_the_plan(sql: &str, rows: usize) {
    let (collation, seen, answered) = run(sql, GENERAL_CI);
    // No text column decides these plans: the connection's collation does.
    assert_eq!(
        collation,
        Collation::from_mysql_name(GENERAL_CI).expect("general_ci")
    );
    assert_ne!(collation, Collation::default());
    assert!(!seen.is_empty(), "{sql}: no scan opened");
    assert!(
        seen.iter().all(|given| *given == Some(collation)),
        "{sql}: scans were given {seen:?}, the plan resolved {collation:?}"
    );
    assert_eq!(answered, rows, "{sql}");
}

#[test]
fn a_scan_compares_pushed_text_under_the_connection_collation() {
    assert_every_scan_compares_under_the_plan(
        "SELECT id, label FROM items WHERE CAST(n AS CHAR) = '5 '",
        2,
    );
}

#[test]
fn a_subquery_scan_compares_pushed_text_under_the_connection_collation() {
    assert_every_scan_compares_under_the_plan(
        "SELECT id FROM items WHERE n IN (SELECT n FROM items WHERE CONCAT(n, '') = '5 ')",
        2,
    );
}

#[test]
fn a_joined_scan_compares_pushed_text_under_the_connection_collation() {
    assert_every_scan_compares_under_the_plan(
        "SELECT a.id, b.id FROM items a JOIN items b ON b.id = a.n \
         WHERE CAST(b.n AS CHAR) IN ('9 ', '7 ')",
        3,
    );
}
