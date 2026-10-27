//! A WHERE conjunct that no null-extended row can pass turns the LEFT join
//! producing that row into an inner join, so the conjunct filters the
//! joined relation itself instead of the fanned-out chain above it, and an
//! EXISTS test runs below the joins it does not read. Each answer here is
//! the one the chain gives with the predicate applied last; the predicates
//! that a null-extended row can pass keep their LEFT joins. Distinct-only
//! aggregates over a LEFT join reached through a keyed bridge join the
//! bridge to it first, and answer as the chain written out does.
//!
//! The measurements are `#[ignore]`d, and a repeat answers from the settled
//! memo unless it is off:
//! `PINTAIL_DISABLE_SETTLED_MEMO=1 cargo test --profile recovery -p pintail-exec
//! --test integration null_rejected_outer_join:: -- --ignored --nocapture`.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlan, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
};
use pintail_sql::{Binder, BoundJoinKind, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE: DatabaseId = DatabaseId::new(3);

fn stored(id: u64, values: Vec<Value>) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        values,
        id,
        false,
    )
}

fn int(value: Option<i64>) -> Value {
    value.map_or(Value::Null, Value::Int64)
}

struct Fixture {
    _directories: Vec<tempfile::TempDir>,
    tables: Vec<(TableId, TableStore)>,
    catalog: CatalogSnapshot,
}

/// `members(id, team, since)` belong to `teams(id, org, plan)`, which sit in
/// `orgs(id, tier)` and follow a `plan`; `steps(id, plan)` fan each team out
/// to its plan's steps, and `marks(id, step, member)` record progress.
#[allow(clippy::too_many_lines)] // five tables of fixture data read best inline
fn fixture(members: u64, teams: u64, steps_per_plan: u64) -> Fixture {
    let plans = 7;
    let orgs = 5;
    let schemas = [
        (
            "members",
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "team", DataType::Int64, true),
                Column::new(3, "since", DataType::Int64, true),
            ],
        ),
        (
            "teams",
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "org", DataType::Int64, true),
                Column::new(3, "plan", DataType::Int64, true),
            ],
        ),
        (
            "orgs",
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "tier", DataType::Int64, true),
            ],
        ),
        (
            "steps",
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "plan", DataType::Int64, false),
            ],
        ),
        (
            "marks",
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "step", DataType::Int64, false),
                Column::new(3, "member", DataType::Int64, false),
            ],
        ),
    ];
    let signed = |value: u64| i64::try_from(value).expect("small");
    let steps = plans * steps_per_plan;
    let rows: [Vec<StoredRow>; 5] = [
        // Every ninth member has no team, and team ids run one past the
        // teams table so some members join nothing.
        (1..=members)
            .map(|id| {
                let team = (!id.is_multiple_of(9)).then(|| signed(id % (teams + 1) + 1));
                stored(
                    id,
                    vec![Value::UInt64(id), int(team), int(Some(signed(id % 4)))],
                )
            })
            .collect(),
        // Every sixth team has no org; org ids run one past the orgs table.
        (1..=teams)
            .map(|id| {
                let org = (!id.is_multiple_of(6)).then(|| signed(id % (orgs + 1) + 1));
                stored(
                    id,
                    vec![
                        Value::UInt64(id),
                        int(org),
                        int(Some(signed(id % (plans + 1) + 1))),
                    ],
                )
            })
            .collect(),
        (1..=orgs)
            .map(|id| {
                stored(
                    id,
                    vec![Value::UInt64(id), int((id != 3).then(|| signed(id % 2)))],
                )
            })
            .collect(),
        (1..=steps)
            .map(|id| {
                stored(
                    id,
                    vec![Value::UInt64(id), Value::Int64(signed(id % plans + 1))],
                )
            })
            .collect(),
        (1..=members * 2)
            .map(|id| {
                stored(
                    id,
                    vec![
                        Value::UInt64(id),
                        Value::Int64(signed(id * 7 % steps + 1)),
                        Value::Int64(signed(id % members + 1)),
                    ],
                )
            })
            .collect(),
    ];
    let mut directories = Vec::new();
    let mut tables = Vec::new();
    let mut entries = Vec::new();
    for (index, ((name, columns), rows)) in schemas.into_iter().zip(rows).enumerate() {
        let id = index as u64 + 1;
        let schema = TableSchema::new(1, columns).expect("schema");
        let directory = tempfile::tempdir().expect("directory");
        let count = rows.len() as u64;
        let mut store = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open table");
        store.bulk_ingest_snapshot(rows).expect("rows");
        directories.push(directory);
        tables.push((TableId::new(id), store));
        entries.push(
            TableEntry::new(
                TableId::new(id),
                name,
                schema,
                TableStatistics::with_row_count(count),
            )
            .expect("entry")
            .with_key_columns([1])
            .expect("key"),
        );
    }
    Fixture {
        _directories: directories,
        tables,
        catalog: CatalogSnapshot::new([
            DatabaseEntry::new(DATABASE, "app", entries).expect("database")
        ])
        .expect("catalog"),
    }
}

fn left_joins(plan: &LogicalPlan) -> usize {
    match plan {
        LogicalPlan::Join {
            left, right, kind, ..
        } => usize::from(*kind == BoundJoinKind::Left) + left_joins(left) + left_joins(right),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Distinct { input, .. }
        | LogicalPlan::Derived { input, .. } => left_joins(input),
        LogicalPlan::CrossJoin { inputs } => inputs.iter().map(left_joins).sum(),
        _ => 0,
    }
}

impl Fixture {
    fn optimized(&self, sql: &str) -> LogicalPlan {
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        Optimizer::optimize(LogicalPlanner::plan(bound))
    }

    fn run(&self, sql: &str) -> Vec<Vec<String>> {
        let snapshots = self
            .tables
            .iter()
            .map(|(id, table)| (*id, table.snapshot()))
            .collect::<Vec<_>>();
        let provider = SnapshotScanProvider::new(
            snapshots
                .iter()
                .map(|(id, snapshot)| (DATABASE, *id, snapshot)),
        )
        .expect("provider");
        let physical =
            PhysicalPlanner::plan(self.optimized(sql), Collation::default()).expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
                .expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                rows.push(
                    (0..batch.columns().len())
                        .map(|column| {
                            match batch.column(column).and_then(|column| column.value(row)) {
                                Some(Value::Null) | None => "NULL".to_owned(),
                                Some(Value::Int64(number)) => number.to_string(),
                                Some(Value::UInt64(number)) => number.to_string(),
                                Some(other) => format!("{other:?}"),
                            }
                        })
                        .collect(),
                );
            }
        }
        rows
    }
}

/// The chain every case filters: each member fans out to its plan's steps
/// and their marks, null-extended wherever a link is missing.
const CHAIN: &str = "FROM members m \
     LEFT JOIN teams t ON t.id = m.team \
     LEFT JOIN orgs o ON o.id = t.org \
     LEFT JOIN steps s ON s.plan = t.plan \
     LEFT JOIN marks k ON k.step = s.id AND k.member = m.id";

const SELECT: &str = "SELECT m.id, COUNT(DISTINCT s.id), COUNT(DISTINCT k.id), COUNT(*)";

const GROUP: &str = "GROUP BY m.id ORDER BY m.id";

/// The same rows through a derived table the filter cannot see into: the
/// chain is joined in full and the predicate applied to its output.
fn reference(fixture: &Fixture, predicate: &str) -> Vec<Vec<String>> {
    fixture.run(&format!(
        "SELECT x.mid, COUNT(DISTINCT x.sid), COUNT(DISTINCT x.kid), COUNT(*) FROM \
         (SELECT m.id mid, s.id sid, k.id kid, m.team, t.org, t.plan, o.tier, m.since, \
          o.id oid, s.plan splan {CHAIN} LIMIT 1000000000) x \
         WHERE {predicate} GROUP BY x.mid ORDER BY x.mid"
    ))
}

#[test]
fn a_null_rejecting_predicate_answers_as_the_filtered_chain() {
    let fixture = fixture(300, 20, 4);
    for (predicate, reference_predicate, lefts) in [
        // A constant on the second link: it and the link below it are inner.
        ("o.tier = 1", "x.tier = 1", 2),
        ("t.id = 4", "x.team = 4", 3),
        ("o.id IN (1, 2)", "x.oid IN (1, 2)", 2),
        (
            "o.tier + 1 > 1 AND m.since < 3",
            "x.tier + 1 > 1 AND x.since < 3",
            2,
        ),
        // Both sides of an OR reject the same link.
        ("o.tier = 1 OR o.id = 5", "x.tier = 1 OR x.oid = 5", 2),
        ("s.plan BETWEEN 2 AND 4", "x.splan BETWEEN 2 AND 4", 2),
    ] {
        let sql = format!("{SELECT} {CHAIN} WHERE {predicate} {GROUP}");
        assert_eq!(
            left_joins(&fixture.optimized(&sql)),
            lefts,
            "{predicate}: LEFT joins left"
        );
        let got = fixture.run(&sql);
        assert!(!got.is_empty(), "{predicate}");
        assert_eq!(got, reference(&fixture, reference_predicate), "{predicate}");
    }
}

#[test]
fn a_predicate_a_null_row_can_pass_keeps_the_left_joins() {
    for (predicate, reference_predicate) in [
        ("o.tier IS NULL", "x.tier IS NULL"),
        ("o.tier = 1 OR m.since = 2", "x.tier = 1 OR x.since = 2"),
        ("COALESCE(o.tier, 0) = 0", "COALESCE(x.tier, 0) = 0"),
        ("NOT (o.tier IS NOT NULL)", "NOT (x.tier IS NOT NULL)"),
        ("o.tier <=> NULL", "x.tier <=> NULL"),
        ("IFNULL(t.plan, 1) = 1", "IFNULL(x.plan, 1) = 1"),
    ] {
        // A fixture each: the answers must not come from one another's memo.
        let fixture = fixture(300, 20, 4);
        let sql = format!("{SELECT} {CHAIN} WHERE {predicate} {GROUP}");
        let got = fixture.run(&sql);
        assert_eq!(got, reference(&fixture, reference_predicate), "{predicate}");
    }
    let fixture = fixture(300, 20, 4);
    for predicate in [
        "o.tier IS NULL",
        "COALESCE(o.tier, 0) = 0",
        "o.tier <=> NULL",
    ] {
        let sql = format!("{SELECT} {CHAIN} WHERE {predicate} {GROUP}");
        assert_eq!(left_joins(&fixture.optimized(&sql)), 4, "{predicate}");
    }
}

#[test]
fn a_semi_join_rejects_the_null_extended_links_it_compares() {
    let fixture = fixture(300, 20, 4);
    let sql = format!(
        "{SELECT} {CHAIN} WHERE EXISTS (SELECT 1 FROM marks q WHERE q.step = s.id) {GROUP}"
    );
    let reference = fixture.run(&format!(
        "SELECT x.mid, COUNT(DISTINCT x.sid), COUNT(DISTINCT x.kid), COUNT(*) FROM \
         (SELECT m.id mid, s.id sid, k.id kid {CHAIN} LIMIT 1000000000) x \
         WHERE x.sid IN (SELECT q.step FROM marks q) GROUP BY x.mid ORDER BY x.mid"
    ));
    assert_ne!(reference, [] as [std::vec::Vec<std::string::String>; 0]);
    assert_eq!(
        outermost_join(&fixture.optimized(&sql)),
        Some(BoundJoinKind::Left),
        "the test moves below the joins that fan out past the step it reads"
    );
    assert_eq!(fixture.run(&sql), reference);
}

#[test]
fn a_predicate_on_a_table_a_derived_table_rescans_keeps_its_left_join() {
    // Unaliased, the outer `teams` and the one the grouped derived table
    // joins inside share a name; the WHERE reads only the outer one, so
    // teams without a counted member keep their null-extended row.
    let fixture = fixture(300, 20, 4);
    let query = |from: &str, outer: &str| {
        format!(
            "SELECT COUNT(*), COUNT(c.n), SUM(c.n) FROM {from} \
             LEFT JOIN (SELECT members.team, COUNT(*) AS n FROM members \
               JOIN teams ON teams.id = members.team \
               WHERE teams.plan > 2 AND members.id < 10 GROUP BY members.team) AS c \
             ON c.team = {outer}.id \
             WHERE {outer}.plan > 2"
        )
    };
    let sql = query("teams", "teams");
    let reference = fixture.run(&query("teams AS u", "u"));
    assert_eq!(left_joins(&fixture.optimized(&sql)), 1);
    assert_ne!(reference[0][0], reference[0][1], "some teams match nothing");
    assert_eq!(fixture.run(&sql), reference);
}

fn outermost_join(plan: &LogicalPlan) -> Option<BoundJoinKind> {
    match plan {
        LogicalPlan::Join { kind, .. } => Some(*kind),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => outermost_join(input),
        _ => None,
    }
}

#[test]
#[ignore = "measurement"]
fn measure_a_semi_join_under_a_fan_out() {
    let fixture = fixture(40_000, 2_000, 60);
    let sql = format!(
        "{SELECT} {CHAIN} WHERE EXISTS (SELECT 1 FROM marks q WHERE q.step = s.id AND q.id < 20) \
         {GROUP}"
    );
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let rows = fixture.run(&sql);
        println!(
            "semi join: {} groups in {:?}",
            rows.len(),
            started.elapsed()
        );
    }
}

#[test]
#[ignore = "measurement"]
fn measure_a_filtered_fan_out() {
    let fixture = fixture(40_000, 2_000, 60);
    let sql = format!("{SELECT} {CHAIN} WHERE t.id = 16 AND o.tier = 1 {GROUP}");
    let expected = reference(&fixture, "x.team = 16 AND x.tier = 1");
    assert_ne!(expected, [] as [std::vec::Vec<std::string::String>; 0]);
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let rows = reference(&fixture, "x.team = 16 AND x.tier = 1");
        let whole = started.elapsed();
        let started = std::time::Instant::now();
        assert_eq!(fixture.run(&sql), rows);
        println!(
            "{} groups: whole chain then filter {whole:?}, filter first {:?}",
            rows.len(),
            started.elapsed()
        );
    }
}

fn nested_left_joins(plan: &LogicalPlan) -> usize {
    match plan {
        LogicalPlan::Join {
            left, right, kind, ..
        } => {
            usize::from(
                *kind == BoundJoinKind::Left && matches!(right.as_ref(), LogicalPlan::Join { .. }),
            ) + nested_left_joins(left)
                + nested_left_joins(right)
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Derived { input, .. } => nested_left_joins(input),
        _ => 0,
    }
}

#[test]
fn distinct_aggregates_join_the_bridge_before_the_preserved_side() {
    let fixture = fixture(300, 20, 4);
    let chain = "FROM members m LEFT JOIN teams t ON t.id = m.team \
                 LEFT JOIN steps s ON s.plan = t.plan \
                 LEFT JOIN marks k ON k.step = s.id AND k.member = m.id AND k.id > m.since";
    let bridged = "FROM members m LEFT JOIN teams t ON t.id = m.team \
                   LEFT JOIN orgs o ON o.id = t.org \
                   LEFT JOIN steps s ON s.plan = t.plan \
                   LEFT JOIN marks k ON k.step = s.id AND k.member = m.id \
                   LEFT JOIN marks j ON j.id = k.id + 1";
    for (from, select, nested) in [
        (
            chain,
            "COUNT(DISTINCT k.id), MIN(k.step), MAX(k.id), \
             COUNT(DISTINCT CASE WHEN k.step > 3 THEN k.id END)",
            1,
        ),
        (
            bridged,
            "COUNT(DISTINCT k.id), COUNT(DISTINCT j.member), MAX(o.tier)",
            0,
        ),
        (bridged, "COUNT(DISTINCT k.id), COUNT(DISTINCT j.member)", 1),
        // Rows count: every null-extended row is one.
        (chain, "COUNT(DISTINCT k.id), COUNT(*)", 0),
        // A value present on null-extended rows.
        (chain, "COUNT(DISTINCT COALESCE(k.id, 0))", 0),
        // The bridge read by the aggregate.
        (chain, "COUNT(DISTINCT k.id), MAX(s.id)", 0),
        (chain, "SUM(k.id)", 0),
    ] {
        let sql = format!("SELECT m.id, {select} {from} GROUP BY m.id ORDER BY m.id");
        // Grouped by a value of the nest, a missing match is its own group.
        let by_target = format!("SELECT m.id, k.step, {select} {from} GROUP BY m.id, k.step");
        assert_eq!(
            nested_left_joins(&fixture.optimized(&by_target)),
            0,
            "{select}"
        );
        assert_eq!(
            nested_left_joins(&fixture.optimized(&sql)),
            nested,
            "{select}"
        );
        // COUNT(*) reads every row, so the same query with it added keeps
        // the chain as written.
        let reference = fixture
            .run(&format!(
                "SELECT m.id, {select}, COUNT(*) {from} GROUP BY m.id ORDER BY m.id"
            ))
            .into_iter()
            .map(|mut row| {
                row.pop();
                row
            })
            .collect::<Vec<_>>();
        assert_eq!(reference.len(), 300, "{select}");
        assert_eq!(fixture.run(&sql), reference, "{select}");
    }
}

#[test]
#[ignore = "measurement"]
fn measure_a_nested_bridge() {
    let fixture = fixture(40_000, 2_000, 60);
    let from = "FROM members m LEFT JOIN teams t ON t.id = m.team \
                LEFT JOIN steps s ON s.plan = t.plan \
                LEFT JOIN marks k ON k.step = s.id AND k.member = m.id";
    let nested = format!("SELECT m.id, COUNT(DISTINCT k.id) {from} GROUP BY m.id");
    let chained = format!("SELECT m.id, COUNT(DISTINCT k.id), COUNT(*) {from} GROUP BY m.id");
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let rows = fixture.run(&chained).len();
        let whole = started.elapsed();
        let started = std::time::Instant::now();
        assert_eq!(fixture.run(&nested).len(), rows);
        println!(
            "{rows} groups: chain as written {whole:?}, bridge nested {:?}",
            started.elapsed()
        );
    }
}
