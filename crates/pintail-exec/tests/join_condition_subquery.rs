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
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [
            TableEntry::new(
                class_id,
                "classes",
                class_schema(),
                TableStatistics::with_row_count(CLASSES),
            )
            .expect("classes entry"),
            TableEntry::new(
                submission_id,
                "submissions",
                submission_schema(),
                TableStatistics::with_row_count(SECTIONS * MEMBERS_PER_SECTION),
            )
            .expect("submissions entry"),
            TableEntry::new(
                membership_id,
                "memberships",
                membership_schema(),
                TableStatistics::with_row_count(SECTIONS * MEMBERS_PER_SECTION),
            )
            .expect("memberships entry"),
        ],
    )
    .expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
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

/// An OUTER join's ON is a different question, and the rewrite does not
/// touch it: the predicate decides which right rows match, while the left
/// row survives either way, so hoisting it to the outer scope would drop the
/// null-extended rows a LEFT JOIN exists to keep.
#[test]
fn a_correlated_in_stays_dependent_in_an_outer_join_condition() {
    let fixture = fixture();

    let (rows, executions) = measure(
        &fixture,
        "SELECT COUNT(*) AS n FROM classes c \
         LEFT JOIN submissions s ON s.item_id = c.item_id \
         AND s.member_id IN ( \
           SELECT m.member_id FROM memberships m WHERE m.section_id = c.section_id \
         )",
    );

    assert!(rows > 0, "the outer join emits a row for every left row");
    assert!(
        executions > 0,
        "an outer join's ON is not hoistable, so this shape still resolves \
         per distinct correlation value; when a rewrite plants the semi-join \
         under the right input this becomes 0 and the assertion should flip",
    );
    println!("correlated IN under LEFT JOIN ON: {executions} inner executions");
}
