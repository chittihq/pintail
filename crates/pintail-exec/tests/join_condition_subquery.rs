//! Where a correlated subquery sits decides whether it decorrelates.
//!
//! An INNER join's ON and the WHERE clause filter the same rows, so a
//! correlated `IN` or `EXISTS` written in either is offered to the
//! decorrelation rewrites and becomes a semi-join. An OUTER join's ON does
//! not filter the same rows - it decides which right rows match, while the
//! left row survives either way - so a subquery there that reaches the
//! join's left side widens the right input instead, and one correlated to
//! the right side alone resolves per correlation value on the dependent path.
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

/// Left rows, and matched right rows, that an outer join scoped by a
/// membership list returns: each class's submissions for its item whose
/// member holds one of the first `limit` memberships in the class's own
/// section. Computed from the fixture's formulas, not by the engine.
fn scoped_join_answer(limit: u64) -> (usize, usize) {
    let people = SECTIONS * MEMBERS_PER_SECTION;
    let member = |submission: u64| submission % people;
    let mut rows = 0;
    let mut matched = 0;
    for class in 1..=CLASSES {
        let found = (1..=people)
            .filter(|submission| submission % 60 == class % 60)
            .filter(|submission| {
                (1..=limit).any(|membership| {
                    membership % people == member(*submission)
                        && membership % SECTIONS == class % SECTIONS
                })
            })
            .count();
        rows += found.max(1);
        matched += found;
    }
    (rows, matched)
}

/// An OUTER join's ON is a different question: the left row survives
/// whether or not the membership holds. A subquery correlated to the
/// join's LEFT side is answered by widening the right input with the
/// DISTINCT membership pairs, so it runs no dependent executions, and it
/// must return exactly the rows the fixture's formulas say - including the
/// null-extended classes no membership reaches.
#[test]
fn an_outer_join_condition_correlated_to_its_left_side_widens_the_right_input() {
    let fixture = fixture();
    let (rows, matched) = scoped_join_answer(100);
    assert!(
        rows > matched && matched > 0,
        "the fixture leaves some classes unmatched and matches others",
    );
    let membership_in = "s.member_id IN ( \
           SELECT m.member_id FROM memberships m \
           WHERE m.section_id = c.section_id AND m.membership_id <= 100)";
    let membership_exists = "EXISTS ( \
           SELECT 1 FROM memberships m WHERE m.member_id = s.member_id \
           AND c.section_id = m.section_id AND m.membership_id <= 100)";
    for predicate in [membership_in, membership_exists] {
        let (all_rows, executions) = measure(
            &fixture,
            &format!(
                "SELECT c.class_id, s.submission_id FROM classes c \
                 LEFT JOIN submissions s ON s.item_id = c.item_id AND {predicate}"
            ),
        );
        assert_eq!(
            all_rows, rows,
            "every class, null-extended when unmatched: {predicate}"
        );
        assert_eq!(
            executions, 0,
            "the widened join runs no dependent executions: {predicate}"
        );
        let (matched_rows, _) = measure(
            &fixture,
            &format!(
                "SELECT c.class_id, s.submission_id FROM classes c \
                 LEFT JOIN submissions s ON s.item_id = c.item_id AND {predicate} \
                 WHERE s.submission_id IS NOT NULL"
            ),
        );
        assert_eq!(
            matched_rows, matched,
            "only the scoped submissions match: {predicate}"
        );
    }

    // The pair table is the rewrite's, not the query's: `*` shows the two
    // tables the statement names and nothing else.
    let catalog = catalog_of(&fixture);
    let statement = parse_statement(&format!(
        "SELECT * FROM classes c LEFT JOIN submissions s \
         ON s.item_id = c.item_id AND {membership_in}"
    ))
    .expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    assert_eq!(
        bound.projection.len(),
        6,
        "classes' three columns and submissions' three"
    );
}

/// Correlating to the RIGHT side alone needs nothing from the left, so it
/// answers on the dependent path, one execution per correlation value, as
/// it did before the widening existed.
#[test]
fn an_outer_join_condition_correlated_to_its_right_side_stays_dependent() {
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
}

/// Left rows, and matched right rows, of `classes LEFT JOIN submissions` on
/// the item plus `accepts`, over the classes in `classes`. Computed from the
/// fixture's formulas, not by the engine.
fn outer_join_answer(
    classes: std::ops::RangeInclusive<u64>,
    accepts: impl Fn(u64, u64) -> bool,
) -> (usize, usize) {
    let people = SECTIONS * MEMBERS_PER_SECTION;
    let mut rows = 0;
    let mut matched = 0;
    for class in classes {
        let found = (1..=people)
            .filter(|submission| submission % 60 == class % 60)
            .filter(|submission| accepts(class, *submission))
            .count();
        rows += found.max(1);
        matched += found;
    }
    (rows, matched)
}

/// Whether a class accepts a submission, by the fixture's formulas.
type Accepts<'a> = dyn Fn(u64, u64) -> bool + 'a;

/// Whether the submission's member holds one of the first `limit`
/// memberships in a section `section` accepts.
fn holds_membership(submission: u64, limit: u64, section: impl Fn(u64) -> bool) -> bool {
    let people = SECTIONS * MEMBERS_PER_SECTION;
    (1..=limit).any(|membership| {
        membership % people == submission % people && section(membership % SECTIONS)
    })
}

/// The shapes the widening does not take run on the dependent join path,
/// which resolves the subquery against each candidate pair. Each must
/// return the rows the fixture's formulas say, null-extended classes
/// included: classes 100 to 123 mix sections no early membership reaches
/// with sections it does.
#[test]
fn an_outer_join_condition_answers_what_it_cannot_widen() {
    let fixture = fixture();
    let classes = 100..=123;
    let in_section = |class: u64, submission: u64| {
        holds_membership(submission, 100, |section| section == class % SECTIONS)
    };
    let below_section = |class: u64, submission: u64| {
        holds_membership(submission, 100, |section| section < class % SECTIONS)
    };
    let shapes: [(&str, &Accepts<'_>); 6] = [
        (
            "s.member_id NOT IN (SELECT m.member_id FROM memberships m \
             WHERE m.section_id = c.section_id AND m.membership_id <= 100)",
            &|class, submission| !in_section(class, submission),
        ),
        (
            "NOT EXISTS (SELECT 1 FROM memberships m WHERE m.member_id = s.member_id \
             AND m.section_id = c.section_id AND m.membership_id <= 100)",
            &|class, submission| !in_section(class, submission),
        ),
        (
            "EXISTS (SELECT 1 FROM memberships m WHERE m.member_id = s.member_id \
             AND m.section_id < c.section_id AND m.membership_id <= 100)",
            &below_section,
        ),
        (
            "s.member_id IN (SELECT m.member_id FROM memberships m \
             JOIN classes c2 ON c2.section_id = m.section_id \
             WHERE c2.class_id = c.class_id AND m.membership_id <= 100)",
            &in_section,
        ),
        (
            "s.member_id IN (SELECT m.member_id FROM memberships m \
             WHERE m.section_id = c.section_id AND m.membership_id <= 100 \
             GROUP BY m.member_id HAVING COUNT(*) >= 1)",
            &in_section,
        ),
        (
            "s.member_id NOT IN (SELECT m.member_id FROM memberships m \
             WHERE m.section_id < c.section_id AND m.membership_id <= 100)",
            &|class, submission| !below_section(class, submission),
        ),
    ];
    for (predicate, accepts) in shapes {
        let (rows, matched) = outer_join_answer(classes.clone(), accepts);
        let join = format!(
            "SELECT c.class_id, s.submission_id FROM classes c \
             LEFT JOIN submissions s ON s.item_id = c.item_id AND {predicate} \
             WHERE c.class_id BETWEEN 100 AND 123"
        );
        let (all_rows, _) = measure(&fixture, &join);
        assert_eq!(
            all_rows, rows,
            "every class, null-extended when unmatched: {predicate}"
        );
        let (matched_rows, _) =
            measure(&fixture, &format!("{join} AND s.submission_id IS NOT NULL"));
        assert_eq!(
            matched_rows, matched,
            "exactly the accepted submissions: {predicate}"
        );
    }
}
