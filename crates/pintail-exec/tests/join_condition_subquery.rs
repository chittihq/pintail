//! Where a correlated subquery sits decides whether it decorrelates.
//!
//! An INNER join's ON and the WHERE clause filter the same rows, so a
//! correlated `IN` or `EXISTS` written in either is offered to the
//! decorrelation rewrites and becomes a semi-join. An OUTER join's ON does
//! not filter the same rows - it decides which right rows match, while the
//! left row survives either way - so a subquery there still resolves per
//! distinct correlation value on the dependent path.
//!
//! The statements here ask the SAME question of the same rows and differ
//! only in where the correlated `IN` is written. Inner executions are
//! counted, not timed: a decorrelated shape runs zero, so the counter says
//! which path each took without depending on how fast the machine is.
//!
//! The shape is not hypothetical. A report joining several tables, whose
//! outer join carried a correlated `IN` in its ON condition to scope one
//! side to a membership list, ran past a sixty-second client deadline while
//! its siblings answered in under two.

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

const CLASSES: u64 = 240;
const SECTIONS: u64 = 40;
const MEMBERS_PER_SECTION: u64 = 25;

fn class_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "class_id", DataType::UInt64, false),
            Column::new(2, "section_id", DataType::UInt64, false),
            Column::new(3, "item_id", DataType::UInt64, false),
        ],
    )
    .expect("class schema")
}

fn submission_schema() -> TableSchema {
    TableSchema::new(
        2,
        vec![
            Column::new(1, "submission_id", DataType::UInt64, false),
            Column::new(2, "item_id", DataType::UInt64, false),
            Column::new(3, "member_id", DataType::UInt64, false),
        ],
    )
    .expect("submission schema")
}

fn membership_schema() -> TableSchema {
    TableSchema::new(
        3,
        vec![
            Column::new(1, "membership_id", DataType::UInt64, false),
            Column::new(2, "section_id", DataType::UInt64, false),
            Column::new(3, "member_id", DataType::UInt64, false),
        ],
    )
    .expect("membership schema")
}

fn stored(id: u64, values: Vec<Value>) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        values,
        id,
        false,
    )
}

struct Fixture {
    _dirs: Vec<tempfile::TempDir>,
    classes: TableStore,
    submissions: TableStore,
    memberships: TableStore,
}

fn fixture() -> Fixture {
    let mut dirs = Vec::new();
    let mut open = |schema: TableSchema| {
        let dir = tempfile::tempdir().expect("dir");
        let store = TableStore::open(dir.path(), schema, StoreOptions::default()).expect("store");
        dirs.push(dir);
        store
    };
    let mut classes = open(class_schema());
    classes
        .bulk_ingest_snapshot(
            (1..=CLASSES)
                .map(|id| {
                    stored(
                        id,
                        vec![
                            Value::UInt64(id),
                            Value::UInt64(id % SECTIONS),
                            Value::UInt64(id % 60),
                        ],
                    )
                })
                .collect(),
        )
        .expect("classes");

    let mut submissions = open(submission_schema());
    submissions
        .bulk_ingest_snapshot(
            (1..=SECTIONS * MEMBERS_PER_SECTION)
                .map(|id| {
                    stored(
                        id,
                        vec![
                            Value::UInt64(id),
                            Value::UInt64(id % 60),
                            Value::UInt64(id % (SECTIONS * MEMBERS_PER_SECTION)),
                        ],
                    )
                })
                .collect(),
        )
        .expect("submissions");

    let mut memberships = open(membership_schema());
    memberships
        .bulk_ingest_snapshot(
            (1..=SECTIONS * MEMBERS_PER_SECTION)
                .map(|id| {
                    stored(
                        id,
                        vec![
                            Value::UInt64(id),
                            Value::UInt64(id % SECTIONS),
                            Value::UInt64(id % (SECTIONS * MEMBERS_PER_SECTION)),
                        ],
                    )
                })
                .collect(),
        )
        .expect("memberships");

    Fixture {
        _dirs: dirs,
        classes,
        submissions,
        memberships,
    }
}

/// Inner executions are counted in a process-wide counter, so two tests
/// measuring at once would each see the other's work. One at a time.
static COUNTER: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The catalog both paths bind against.
fn catalog_of(_fixture: &Fixture) -> CatalogSnapshot {
    let database = DatabaseEntry::new(
        DatabaseId::new(4),
        "app",
        [
            TableEntry::new(
                TableId::new(41),
                "classes",
                class_schema(),
                TableStatistics::with_row_count(CLASSES),
            )
            .expect("classes entry"),
            TableEntry::new(
                TableId::new(42),
                "submissions",
                submission_schema(),
                TableStatistics::with_row_count(SECTIONS * MEMBERS_PER_SECTION),
            )
            .expect("submissions entry"),
            TableEntry::new(
                TableId::new(43),
                "memberships",
                membership_schema(),
                TableStatistics::with_row_count(SECTIONS * MEMBERS_PER_SECTION),
            )
            .expect("memberships entry"),
        ],
    )
    .expect("database");
    CatalogSnapshot::new([database]).expect("catalog")
}

/// Binds one statement and returns the error text it is refused with.
fn plan_error(fixture: &Fixture, sql: &str) -> String {
    let statement = parse_statement(sql).expect("parse");
    let catalog = catalog_of(fixture);
    match Binder::new(&catalog, Some("app")).bind(&statement) {
        Ok(_) => panic!("expected the statement to be refused"),
        Err(error) => error.to_string(),
    }
}

/// Runs one statement, returning its rows and the inner executions it took.
fn measure(fixture: &Fixture, sql: &str) -> (usize, u64) {
    let _counter = COUNTER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let class_snapshot = fixture.classes.snapshot();
    let submission_snapshot = fixture.submissions.snapshot();
    let membership_snapshot = fixture.memberships.snapshot();
    let database_id = DatabaseId::new(4);
    let (class_id, submission_id, membership_id) =
        (TableId::new(41), TableId::new(42), TableId::new(43));
    let catalog = catalog_of(fixture);
    let provider = SnapshotScanProvider::new([
        (database_id, class_id, &class_snapshot),
        (database_id, submission_id, &submission_snapshot),
        (database_id, membership_id, &membership_snapshot),
    ])
    .expect("provider");

    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let before = dependent_subquery_executions();
    let mut execution =
        Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
            .expect("start");
    let mut rows = 0;
    while let Some(batch) = execution.next_batch().expect("batch") {
        rows += batch.selection().selected_rows().count();
    }
    (rows, dependent_subquery_executions() - before)
}

/// The same membership test in the WHERE clause and in an INNER join's ON.
/// Both are inner joins so the two answers are identical, which is what
/// makes the execution counts comparable.
#[test]
fn a_correlated_in_decorrelates_from_where_and_from_an_inner_join_condition() {
    let fixture = fixture();

    let (where_rows, where_executions) = measure(
        &fixture,
        "SELECT COUNT(*) AS n FROM classes c \
         JOIN submissions s ON s.item_id = c.item_id \
         WHERE s.member_id IN ( \
           SELECT m.member_id FROM memberships m WHERE m.section_id = c.section_id \
         )",
    );

    let (on_rows, on_executions) = measure(
        &fixture,
        "SELECT COUNT(*) AS n FROM classes c \
         JOIN submissions s ON s.item_id = c.item_id \
         AND s.member_id IN ( \
           SELECT m.member_id FROM memberships m WHERE m.section_id = c.section_id \
         )",
    );

    assert_eq!(
        where_rows, on_rows,
        "the two placements ask the same question",
    );
    assert_eq!(
        where_executions, 0,
        "a correlated IN in WHERE decorrelates into a semi-join",
    );
    assert_eq!(
        on_executions, 0,
        "an INNER join's ON filters the rows WHERE filters, so the same \
         predicate decorrelates from there too",
    );
}

/// An OUTER join's ON is a different question. A subquery correlated to the
/// join's LEFT side is refused there: dependent resolution exists only at
/// Filter level, so it ran without the outer context it needs and the join
/// matched too few rows - measured against `MySQL`, three matches reported as
/// one. Correlating to the RIGHT side alone still answers, on the dependent
/// path.
#[test]
fn an_outer_join_condition_refuses_a_subquery_correlated_to_its_left_side() {
    let fixture = fixture();

    let (rows, executions) = measure(
        &fixture,
        "SELECT COUNT(*) AS n FROM classes c \
         LEFT JOIN submissions s ON s.item_id = c.item_id \
         AND s.member_id IN ( \
           SELECT m.member_id FROM memberships m WHERE m.section_id = s.item_id \
         )",
    );
    assert!(rows > 0, "correlating to the right side still answers");
    assert!(
        executions > 0,
        "and does so on the dependent path, one execution per correlation value",
    );

    let refused = plan_error(
        &fixture,
        "SELECT COUNT(*) AS n FROM classes c \
         LEFT JOIN submissions s ON s.item_id = c.item_id \
         AND s.member_id IN ( \
           SELECT m.member_id FROM memberships m WHERE m.section_id = c.section_id \
         )",
    );
    assert!(
        refused.contains("left side"),
        "correlating to the join's left side is refused, not answered: {refused}",
    );
}
