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

const SLOTS: u64 = 240;
const CREWS: u64 = 40;
const MEMBERS_PER_CREW: u64 = 25;

fn slot_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "slot_id", DataType::UInt64, false),
            Column::new(2, "crew_id", DataType::UInt64, false),
            Column::new(3, "item_id", DataType::UInt64, false),
        ],
    )
    .expect("slot schema")
}

fn receipt_schema() -> TableSchema {
    TableSchema::new(
        2,
        vec![
            Column::new(1, "receipt_id", DataType::UInt64, false),
            Column::new(2, "item_id", DataType::UInt64, false),
            Column::new(3, "member_id", DataType::UInt64, false),
        ],
    )
    .expect("receipt schema")
}

fn membership_schema() -> TableSchema {
    TableSchema::new(
        3,
        vec![
            Column::new(1, "membership_id", DataType::UInt64, false),
            Column::new(2, "crew_id", DataType::UInt64, false),
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
    slots: TableStore,
    receipts: TableStore,
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
    let mut slots = open(slot_schema());
    slots
        .bulk_ingest_snapshot(
            (1..=SLOTS)
                .map(|id| {
                    stored(
                        id,
                        vec![
                            Value::UInt64(id),
                            Value::UInt64(id % CREWS),
                            Value::UInt64(id % 60),
                        ],
                    )
                })
                .collect(),
        )
        .expect("slots");

    let mut receipts = open(receipt_schema());
    receipts
        .bulk_ingest_snapshot(
            (1..=CREWS * MEMBERS_PER_CREW)
                .map(|id| {
                    stored(
                        id,
                        vec![
                            Value::UInt64(id),
                            Value::UInt64(id % 60),
                            Value::UInt64(id % (CREWS * MEMBERS_PER_CREW)),
                        ],
                    )
                })
                .collect(),
        )
        .expect("receipts");

    let mut memberships = open(membership_schema());
    memberships
        .bulk_ingest_snapshot(
            (1..=CREWS * MEMBERS_PER_CREW)
                .map(|id| {
                    stored(
                        id,
                        vec![
                            Value::UInt64(id),
                            Value::UInt64(id % CREWS),
                            Value::UInt64(id % (CREWS * MEMBERS_PER_CREW)),
                        ],
                    )
                })
                .collect(),
        )
        .expect("memberships");

    Fixture {
        _dirs: dirs,
        slots,
        receipts,
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
                "slots",
                slot_schema(),
                TableStatistics::with_row_count(SLOTS),
            )
            .expect("slots entry"),
            TableEntry::new(
                TableId::new(42),
                "receipts",
                receipt_schema(),
                TableStatistics::with_row_count(CREWS * MEMBERS_PER_CREW),
            )
            .expect("receipts entry"),
            TableEntry::new(
                TableId::new(43),
                "memberships",
                membership_schema(),
                TableStatistics::with_row_count(CREWS * MEMBERS_PER_CREW),
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
    let slot_snapshot = fixture.slots.snapshot();
    let receipt_snapshot = fixture.receipts.snapshot();
    let membership_snapshot = fixture.memberships.snapshot();
    let database_id = DatabaseId::new(4);
    let (slot_id, receipt_id, membership_id) =
        (TableId::new(41), TableId::new(42), TableId::new(43));
    let catalog = catalog_of(fixture);
    let provider = SnapshotScanProvider::new([
        (database_id, slot_id, &slot_snapshot),
        (database_id, receipt_id, &receipt_snapshot),
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
        "SELECT COUNT(*) AS n FROM slots c \
         JOIN receipts s ON s.item_id = c.item_id \
         WHERE s.member_id IN ( \
           SELECT m.member_id FROM memberships m WHERE m.crew_id = c.crew_id \
         )",
    );

    let (on_rows, on_executions) = measure(
        &fixture,
        "SELECT COUNT(*) AS n FROM slots c \
         JOIN receipts s ON s.item_id = c.item_id \
         AND s.member_id IN ( \
           SELECT m.member_id FROM memberships m WHERE m.crew_id = c.crew_id \
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
/// membership list returns: each slot's receipts for its item whose
/// member holds one of the first `limit` memberships in the slot's own
/// crew. Computed from the fixture's formulas, not by the engine.
fn scoped_join_answer(limit: u64) -> (usize, usize) {
    let people = CREWS * MEMBERS_PER_CREW;
    let member = |receipt: u64| receipt % people;
    let mut rows = 0;
    let mut matched = 0;
    for slot in 1..=SLOTS {
        let found = (1..=people)
            .filter(|receipt| receipt % 60 == slot % 60)
            .filter(|receipt| {
                (1..=limit).any(|membership| {
                    membership % people == member(*receipt)
                        && membership % CREWS == slot % CREWS
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
/// null-extended slots no membership reaches.
#[test]
fn an_outer_join_condition_correlated_to_its_left_side_widens_the_right_input() {
    let fixture = fixture();
    let (rows, matched) = scoped_join_answer(100);
    assert!(
        rows > matched && matched > 0,
        "the fixture leaves some slots unmatched and matches others",
    );
    let membership_in = "s.member_id IN ( \
           SELECT m.member_id FROM memberships m \
           WHERE m.crew_id = c.crew_id AND m.membership_id <= 100)";
    let membership_exists = "EXISTS ( \
           SELECT 1 FROM memberships m WHERE m.member_id = s.member_id \
           AND c.crew_id = m.crew_id AND m.membership_id <= 100)";
    for predicate in [membership_in, membership_exists] {
        let (all_rows, executions) = measure(
            &fixture,
            &format!(
                "SELECT c.slot_id, s.receipt_id FROM slots c \
                 LEFT JOIN receipts s ON s.item_id = c.item_id AND {predicate}"
            ),
        );
        assert_eq!(
            all_rows, rows,
            "every slot, null-extended when unmatched: {predicate}"
        );
        assert_eq!(
            executions, 0,
            "the widened join runs no dependent executions: {predicate}"
        );
        let (matched_rows, _) = measure(
            &fixture,
            &format!(
                "SELECT c.slot_id, s.receipt_id FROM slots c \
                 LEFT JOIN receipts s ON s.item_id = c.item_id AND {predicate} \
                 WHERE s.receipt_id IS NOT NULL"
            ),
        );
        assert_eq!(
            matched_rows, matched,
            "only the scoped receipts match: {predicate}"
        );
    }

    // The pair table is the rewrite's, not the query's: `*` shows the two
    // tables the statement names and nothing else.
    let catalog = catalog_of(&fixture);
    let statement = parse_statement(&format!(
        "SELECT * FROM slots c LEFT JOIN receipts s \
         ON s.item_id = c.item_id AND {membership_in}"
    ))
    .expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    assert_eq!(
        bound.projection.len(),
        6,
        "slots' three columns and receipts' three"
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
        "SELECT COUNT(*) AS n FROM slots c \
         LEFT JOIN receipts s ON s.item_id = c.item_id \
         AND s.member_id IN ( \
           SELECT m.member_id FROM memberships m WHERE m.crew_id = s.item_id \
         )",
    );
    assert!(rows > 0, "correlating to the right side still answers");
    assert!(
        executions > 0,
        "and does so on the dependent path, one execution per correlation value",
    );
}

/// Left rows, and matched right rows, of `slots LEFT JOIN receipts` on
/// the item plus `accepts`, over the slots in `slots`. Computed from the
/// fixture's formulas, not by the engine.
fn outer_join_answer(
    slots: std::ops::RangeInclusive<u64>,
    accepts: impl Fn(u64, u64) -> bool,
) -> (usize, usize) {
    let people = CREWS * MEMBERS_PER_CREW;
    let mut rows = 0;
    let mut matched = 0;
    for slot in slots {
        let found = (1..=people)
            .filter(|receipt| receipt % 60 == slot % 60)
            .filter(|receipt| accepts(slot, *receipt))
            .count();
        rows += found.max(1);
        matched += found;
    }
    (rows, matched)
}

/// Whether a slot accepts a receipt, by the fixture's formulas.
type Accepts<'a> = dyn Fn(u64, u64) -> bool + 'a;

/// Whether the receipt's member holds one of the first `limit`
/// memberships in a crew `crew` accepts.
fn holds_membership(receipt: u64, limit: u64, crew: impl Fn(u64) -> bool) -> bool {
    let people = CREWS * MEMBERS_PER_CREW;
    (1..=limit).any(|membership| {
        membership % people == receipt % people && crew(membership % CREWS)
    })
}

/// The shapes the widening does not take run on the dependent join path,
/// which resolves the subquery against each candidate pair. Each must
/// return the rows the fixture's formulas say, null-extended slots
/// included: slots 100 to 123 mix crews no early membership reaches
/// with crews it does.
#[test]
fn an_outer_join_condition_answers_what_it_cannot_widen() {
    let fixture = fixture();
    let slots = 100..=123;
    let in_crew = |slot: u64, receipt: u64| {
        holds_membership(receipt, 100, |crew| crew == slot % CREWS)
    };
    let below_crew = |slot: u64, receipt: u64| {
        holds_membership(receipt, 100, |crew| crew < slot % CREWS)
    };
    let shapes: [(&str, &Accepts<'_>); 6] = [
        (
            "s.member_id NOT IN (SELECT m.member_id FROM memberships m \
             WHERE m.crew_id = c.crew_id AND m.membership_id <= 100)",
            &|slot, receipt| !in_crew(slot, receipt),
        ),
        (
            "NOT EXISTS (SELECT 1 FROM memberships m WHERE m.member_id = s.member_id \
             AND m.crew_id = c.crew_id AND m.membership_id <= 100)",
            &|slot, receipt| !in_crew(slot, receipt),
        ),
        (
            "EXISTS (SELECT 1 FROM memberships m WHERE m.member_id = s.member_id \
             AND m.crew_id < c.crew_id AND m.membership_id <= 100)",
            &below_crew,
        ),
        (
            "s.member_id IN (SELECT m.member_id FROM memberships m \
             JOIN slots c2 ON c2.crew_id = m.crew_id \
             WHERE c2.slot_id = c.slot_id AND m.membership_id <= 100)",
            &in_crew,
        ),
        (
            "s.member_id IN (SELECT m.member_id FROM memberships m \
             WHERE m.crew_id = c.crew_id AND m.membership_id <= 100 \
             GROUP BY m.member_id HAVING COUNT(*) >= 1)",
            &in_crew,
        ),
        (
            "s.member_id NOT IN (SELECT m.member_id FROM memberships m \
             WHERE m.crew_id < c.crew_id AND m.membership_id <= 100)",
            &|slot, receipt| !below_crew(slot, receipt),
        ),
    ];
    for (predicate, accepts) in shapes {
        let (rows, matched) = outer_join_answer(slots.clone(), accepts);
        let join = format!(
            "SELECT c.slot_id, s.receipt_id FROM slots c \
             LEFT JOIN receipts s ON s.item_id = c.item_id AND {predicate} \
             WHERE c.slot_id BETWEEN 100 AND 123"
        );
        let (all_rows, _) = measure(&fixture, &join);
        assert_eq!(
            all_rows, rows,
            "every slot, null-extended when unmatched: {predicate}"
        );
        let (matched_rows, _) =
            measure(&fixture, &format!("{join} AND s.receipt_id IS NOT NULL"));
        assert_eq!(
            matched_rows, matched,
            "exactly the accepted receipts: {predicate}"
        );
    }
}
