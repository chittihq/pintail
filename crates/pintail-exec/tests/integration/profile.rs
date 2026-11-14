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

/// Runs `sql` profiled and returns the rows and the profile.
fn profiled(
    fixture: &Fixture,
    provider: &SnapshotScanProvider<'_>,
    sql: &str,
) -> (Vec<String>, pintail_exec::ExecutionProfile) {
    let bound = Binder::new(&fixture.catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start_profiled(physical, provider, 64 << 20, None, Collation::default())
            .expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("batch") {
        for index in batch.selection().selected_rows() {
            let values: Vec<_> = batch
                .columns()
                .iter()
                .map(|column| column.value(index).expect("value"))
                .collect();
            rows.push(format!("{values:?}"));
        }
    }
    rows.sort();
    (rows, execution.profile().expect("profile"))
}

fn plain(fixture: &Fixture, provider: &SnapshotScanProvider<'_>, sql: &str) -> Vec<String> {
    let bound = Binder::new(&fixture.catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, provider, 64 << 20, Collation::default()).expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("batch") {
        for index in batch.selection().selected_rows() {
            let values: Vec<_> = batch
                .columns()
                .iter()
                .map(|column| column.value(index).expect("value"))
                .collect();
            rows.push(format!("{values:?}"));
        }
    }
    rows.sort();
    rows
}

#[test]
fn profiling_does_not_change_the_execution_path() {
    let fixture = Fixture::new();
    let snapshot = fixture.table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    // A bare-scan sum folds from the segment summaries and decodes nothing;
    // the recorder around the scan must not turn it into a scan.
    let sql = "SELECT SUM(amount), MIN(grp), MAX(grp) FROM events";
    let (rows, profile) = profiled(&fixture, &provider, sql);
    assert_eq!(rows, plain(&fixture, &provider, sql));
    let scan = profile
        .operators
        .iter()
        .find(|node| node.label.starts_with("Scan"))
        .expect("scan");
    assert_eq!(
        scan.batches,
        0,
        "the fold must not pull the scan: {}",
        profile.render()
    );
    let stats = provider
        .scan_stats(DatabaseId::new(1), TableId::new(1))
        .unwrap_or_default();
    assert_eq!(
        stats.blocks_decoded, 0,
        "the fold decoded blocks under profiling"
    );
}

/// One operator of an accounting shape: its depth, whether it was pulled,
/// and the depth of the operator that pulled it while being built, when
/// its pulls happened there rather than after the tree stood.
type Shape = (usize, bool, Option<usize>);

/// Self times must account for the root exactly once. The check runs on a
/// fixed clock: keeping the tree a real execution produced (or one written
/// by hand), every operator is given a known time to build and, when it
/// was pulled, to run, the recorded times are replaced by what those add
/// up to, and each operator's self time must then be exactly its own -
/// which also makes them sum to the root. Measured times cannot say this:
/// a built-only operator's construction counted twice is a microsecond on
/// an idle host and milliseconds on a busy one.
fn assert_shape_accounts_for_the_root_once(
    template: &pintail_exec::ExecutionProfile,
    shape: &[Shape],
) {
    use std::time::Duration;
    let count = shape.len();
    let millis = |value: usize| Duration::from_millis(u64::try_from(value).expect("small"));
    let built = |index: usize| millis(3 + 2 * index);
    let run = |index: usize| {
        if shape[index].1 {
            millis(101 + 10 * index)
        } else {
            Duration::ZERO
        }
    };
    let below = |index: usize| -> Vec<usize> {
        (index + 1..count)
            .take_while(|&other| shape[other].0 > shape[index].0)
            .collect()
    };
    let mut profile = template.clone();
    profile.operators = (0..count)
        .map(|index| {
            let (depth, ran, when) = shape[index];
            let descendants = below(index);
            // Building an operator builds everything below it, and holds
            // the pulls made meanwhile, by it or by one built inside it.
            let construction = built(index)
                + descendants
                    .iter()
                    .map(|&other| {
                        built(other)
                            + if shape[other].2.is_some_and(|builder| builder >= depth) {
                                run(other)
                            } else {
                                Duration::ZERO
                            }
                    })
                    .sum::<Duration>();
            // A pull of an operator holds the pulls it makes in turn: those
            // of the operators below it that ran in the same phase.
            let pulls = run(index)
                + descendants
                    .iter()
                    .filter(|&&other| shape[other].2 == when)
                    .map(|&other| run(other))
                    .sum::<Duration>();
            pintail_exec::OperatorProfile {
                label: format!("operator {index}"),
                depth,
                batches: u64::from(ran),
                construction,
                inclusive: construction + if ran { pulls } else { Duration::ZERO },
                built_pulls: match when {
                    Some(builder) if ran => vec![(builder, pulls)],
                    _ => Vec::new(),
                },
                ..pintail_exec::OperatorProfile::default()
            }
        })
        .collect();
    for index in 0..count {
        assert_eq!(
            profile.exclusive(index),
            built(index) + run(index),
            "operator {index} is charged its own time and nothing else:\n{}",
            profile.render()
        );
    }
    let total: Duration = (0..count).map(|index| profile.exclusive(index)).sum();
    assert_eq!(
        total,
        profile.operators[0].inclusive,
        "{}",
        profile.render()
    );
}

/// The same check over the tree a real execution produced.
fn assert_self_times_account_for_the_root_once(measured: &pintail_exec::ExecutionProfile) {
    let shape: Vec<Shape> = measured
        .operators
        .iter()
        .map(|node| {
            (
                node.depth,
                node.batches > 0,
                node.built_pulls.first().map(|(builder, _)| *builder),
            )
        })
        .collect();
    assert!(
        shape[0].1,
        "the root produced the rows: {}",
        measured.render()
    );
    assert_shape_accounts_for_the_root_once(measured, &shape);
}

#[test]
fn a_fused_join_and_a_built_join_keep_the_accounting_consistent() {
    let fixture = Fixture::new();
    let snapshot = fixture.table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    // The fused inner join: the aggregate pulls the join's inputs directly,
    // so the join records nothing of its own and says so, and self times
    // still add up to no more than the root.
    let sql = "SELECT b.grp, COUNT(*) FROM events a JOIN events b ON b.id = a.id GROUP BY b.grp";
    let (rows, profile) = profiled(&fixture, &provider, sql);
    assert_eq!(rows, plain(&fixture, &provider, sql));
    let join = profile
        .operators
        .iter()
        .position(|node| node.label.starts_with("HashJoin"))
        .expect("join");
    if profile.operators[join].batches == 0 {
        assert!(
            profile.operators[join].note.is_some(),
            "a join that never ran on its own must say why: {}",
            profile.render()
        );
    }
    assert_self_times_account_for_the_root_once(&profile);

    // A nested-loop join runs while it is built: its time lands on the
    // join's own line, not only on the scans beneath it. (A count over it
    // would be taken without running the join at all.)
    let sql = "SELECT MAX(a.grp) FROM (SELECT grp FROM events LIMIT 40) a \
               JOIN (SELECT grp FROM events LIMIT 40) b ON a.grp > b.grp";
    let (rows, profile) = profiled(&fixture, &provider, sql);
    assert_eq!(rows, plain(&fixture, &provider, sql));
    let nested = profile
        .operators
        .iter()
        .find(|node| node.label.starts_with("NestedLoopJoin"))
        .expect("nested loop");
    assert!(
        !nested.construction.is_zero() && nested.inclusive >= nested.construction,
        "construction time is attributed: {}",
        profile.render()
    );
    let rendered = profile.render();
    assert!(rendered.contains("build="), "{rendered}");
    // Its inputs were pulled while it was being built, and say so.
    let position = profile
        .operators
        .iter()
        .position(|node| node.label.starts_with("NestedLoopJoin"))
        .expect("nested loop");
    let depth = profile.operators[position].depth;
    assert!(
        profile.operators[position + 1..]
            .iter()
            .take_while(|node| node.depth > depth)
            .any(|node| node
                .built_pulls
                .iter()
                .any(|(builder, _)| *builder == depth)),
        "the join's inputs record the pulls made while it was built: {rendered}"
    );
    assert_self_times_account_for_the_root_once(&profile);

    // Shapes a query does not reliably produce, on the same fixed clock.
    // A join that ran its inputs while it was built and was never pulled
    // itself, because the other side of the join above it was empty:
    assert_shape_accounts_for_the_root_once(
        &profile,
        &[
            (0, true, None),
            (1, true, None),
            (1, false, None),
            (2, true, Some(1)),
            (2, true, Some(1)),
        ],
    );
    // the same join pulled afterwards;
    assert_shape_accounts_for_the_root_once(
        &profile,
        &[
            (0, true, None),
            (1, true, None),
            (2, true, Some(1)),
            (2, true, Some(1)),
        ],
    );
    // one that took the inputs of a fused operator below it while built;
    assert_shape_accounts_for_the_root_once(
        &profile,
        &[
            (0, true, None),
            (1, false, None),
            (2, false, None),
            (3, true, Some(1)),
        ],
    );
    // a root that did the same;
    assert_shape_accounts_for_the_root_once(
        &profile,
        &[(0, true, None), (1, false, None), (2, true, Some(0))],
    );
    // and one built-time join inside another that was never pulled.
    assert_shape_accounts_for_the_root_once(
        &profile,
        &[
            (0, true, None),
            (1, false, None),
            (2, true, Some(1)),
            (3, true, Some(2)),
            (2, true, Some(1)),
        ],
    );
}
