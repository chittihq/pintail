//! Where a correlated subquery sits decides whether it decorrelates.
//!
//! The binder offers a correlated `IN` or `EXISTS` to the decorrelation
//! rewrites only from the WHERE clause. A subquery in a JOIN's ON condition
//! is bound like any other expression and reaches the dependent path, which
//! plans and executes the inner query once per distinct correlation value.
//!
//! The two statements here ask the SAME question of the same rows and differ
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

/// Runs one statement, returning its rows and the inner executions it took.
fn measure(fixture: &Fixture, sql: &str) -> (usize, u64) {
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

/// The same membership test, written in the WHERE clause and in a JOIN's ON
/// condition. Both are inner joins so the two answers are identical, which
/// is what makes the execution counts comparable.
#[test]
fn a_correlated_in_decorrelates_from_where_and_not_from_a_join_condition() {
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
        "a correlated IN in WHERE decorrelates into a join and runs no inner query",
    );
    assert!(
        on_executions > 0,
        "a correlated IN in a JOIN condition is never offered to the decorrelation \
         rewrites, so it resolves per outer tuple; if this is 0 the rewrite now \
         reaches ON conditions and this test should assert that instead",
    );
    println!("correlated IN: WHERE {where_executions} inner executions, ON {on_executions}");
}
