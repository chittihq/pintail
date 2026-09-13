//! A join hands its build table back when it finishes, not when the query is
//! dropped.
//!
//! The build is dead the moment the probe is exhausted: a left or anti join
//! emits its unmatched build rows before that point, so nothing reads the
//! table afterwards. It was nevertheless kept, and kept charged, until the
//! whole execution was dropped - which meant an aggregate or a sort above the
//! join ran under a ceiling holding a hash table nobody could read.
//!
//! What this pins is the part that is easy to get wrong twice: the
//! reservation coming back is only bookkeeping unless the table is freed with
//! it, and neither is worth anything unless it happens while the rest of the
//! query is still running.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const BUILD_ROWS: u64 = 20_000;
const PROBE_ROWS: u64 = 60_000;

fn dimension_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

/// The fact table keys on its own id and joins on `ref_id`, so every probe
/// row is distinct and the fan-out onto the build is real.
fn fact_schema() -> TableSchema {
    TableSchema::new(
        2,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "ref_id", DataType::UInt64, false),
            Column::new(3, "note", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn dimension_row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Utf8(format!("label-{id:06}"))],
        id + 1,
        false,
    )
}

fn fact_row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::UInt64(id % BUILD_ROWS),
            Value::Utf8(format!("note-{id:06}")),
        ],
        id + 1,
        false,
    )
}

struct Fixture {
    _directory: tempfile::TempDir,
    dimension: TableStore,
    fact: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut dimension = TableStore::open(
            directory.path().join("dimension"),
            dimension_schema(),
            StoreOptions::default(),
        )
        .expect("dimension");
        dimension
            .bulk_ingest_snapshot((0..BUILD_ROWS).map(dimension_row).collect())
            .expect("build rows");
        let mut fact = TableStore::open(
            directory.path().join("fact"),
            fact_schema(),
            StoreOptions::default(),
        )
        .expect("fact");
        fact.bulk_ingest_snapshot((0..PROBE_ROWS).map(fact_row).collect())
            .expect("probe rows");
        let entries = [
            TableEntry::new(
                TableId::new(1),
                "dimension",
                dimension_schema(),
                TableStatistics::with_row_count(BUILD_ROWS),
            )
            .expect("entry")
            .with_key_columns([1])
            .expect("key"),
            TableEntry::new(
                TableId::new(2),
                "fact",
                fact_schema(),
                TableStatistics::with_row_count(PROBE_ROWS),
            )
            .expect("entry")
            .with_key_columns([1])
            .expect("key"),
        ];
        Self {
            _directory: directory,
            dimension,
            fact,
            catalog: CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", entries).expect("database")
            ])
            .expect("catalog"),
        }
    }
}

#[test]
fn a_join_hands_its_build_back_before_the_query_ends() {
    let fixture = Fixture::new();
    let dimension = fixture.dimension.snapshot();
    let fact = fixture.fact.snapshot();
    let provider = SnapshotScanProvider::new([
        (DatabaseId::new(1), TableId::new(1), &dimension),
        (DatabaseId::new(1), TableId::new(2), &fact),
    ])
    .expect("provider");
    let sql = "SELECT f.note, d.label FROM fact f JOIN dimension d ON d.id = f.ref_id";
    let bound = Binder::new(&fixture.catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 1 << 30, Collation::default()).expect("start");

    let mut peak = 0;
    let mut rows = 0;
    while let Some(batch) = execution.next_batch().expect("batch") {
        rows += batch.selection().selected_rows().count();
        peak = peak.max(execution.memory().used());
    }
    // The query is not over: the execution is alive and would serve another
    // operator above this one. The join is, though, and what it held is back.
    let after = execution.memory().used();

    assert_eq!(
        u64::try_from(rows).expect("row count"),
        PROBE_ROWS,
        "every probe row matched"
    );
    assert!(peak > 0, "the join charged something while it ran");
    assert!(
        after * 4 < peak,
        "the join still holds {after} bytes of the {peak} it peaked at, after serving its last row"
    );
}
