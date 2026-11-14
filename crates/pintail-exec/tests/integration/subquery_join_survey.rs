//! What correlated subqueries and joins without an equality key cost, shape
//! by shape, over one invented table read as `a` (the outer or left side)
//! and `b` (the inner or right side).
//!
//! Three families, named by their prefix: `t1` a correlated subquery in the
//! select list or WHERE, `t2` a subquery in a LEFT join's ON that reaches
//! the preserved side, `t3` a join whose only link is an inequality. Each
//! line reports the wall time beside what does not depend on the machine:
//! the inner executions the dependent path ran, the candidate pairs a join
//! with no hashable key tested, and the reasons a subquery was left to the
//! per-row path. `plan_share` splits one dependent execution into planning
//! its inner query, starting it and running it.
//!
//! Sizes default small so every shape runs in the unit gate, where the
//! only claim is that each one answers.
//! `SURVEY_LEFT=20000 SURVEY_RIGHT=8000 SURVEY_KEYS=1000 SURVEY_ONLY=t1,t2
//! cargo test --release --test integration subquery_join_survey --
//! --nocapture` prints the table at a size where the times mean something
//! (`benchmark/evidence/inequality-join-search.md`,
//! `dependent-subquery-plan-share.md`).

use std::time::Instant;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    dependent_subquery_executions, take_dependent_declines, take_exec_counters,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "k", DataType::Int64, false),
            Column::new(3, "x", DataType::Int64, true),
            Column::new(4, "lo", DataType::Int64, true),
            Column::new(5, "hi", DataType::Int64, true),
            Column::new(
                6,
                "amt",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                true,
            ),
            Column::new(7, "t0", DataType::DateTime64 { fsp: 0 }, true),
            Column::new(8, "t3", DataType::DateTime64 { fsp: 3 }, true),
            Column::new(9, "s", DataType::Utf8, true),
            Column::new(10, "s2", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

fn minute(id: u64) -> String {
    format!(
        "2024-01-{:02} {:02}:{:02}:00",
        1 + id / 1440,
        (id / 60) % 24,
        id % 60
    )
}

fn rows(count: u64, keys: u64) -> Vec<StoredRow> {
    (1..=count)
        .map(|id| {
            let signed = i64::try_from(id).expect("small");
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                vec![
                    Value::UInt64(id),
                    Value::Int64(signed % i64::try_from(keys).expect("small")),
                    Value::Int64(signed),
                    Value::Int64(signed - 1),
                    Value::Int64(signed + 1),
                    Value::Utf8(format!("{id}.50")),
                    Value::Utf8(minute(id)),
                    Value::Utf8(format!("{}.500", minute(id))),
                    Value::Utf8(format!("n{id:06}")),
                    Value::Utf8(format!("n{:06}", id + 2)),
                ],
                id,
                false,
            )
        })
        .collect()
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[test]
#[allow(clippy::too_many_lines)]
fn survey() {
    let left_rows = env("SURVEY_LEFT", 300);
    let right_rows = env("SURVEY_RIGHT", 300);
    let keys = env("SURVEY_KEYS", 50);
    let only = std::env::var("SURVEY_ONLY").unwrap_or_default();
    let a_dir = tempfile::tempdir().expect("dir");
    let b_dir = tempfile::tempdir().expect("dir");
    let mut a = TableStore::open(a_dir.path(), schema(), StoreOptions::default()).expect("open");
    a.bulk_ingest_snapshot(rows(left_rows, keys)).expect("a");
    let mut b = TableStore::open(b_dir.path(), schema(), StoreOptions::default()).expect("open");
    b.bulk_ingest_snapshot(rows(right_rows, keys)).expect("b");
    let (a_snapshot, b_snapshot) = (a.snapshot(), b.snapshot());
    let database_id = DatabaseId::new(3);
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        database_id,
        "app",
        [
            TableEntry::new(
                TableId::new(31),
                "a",
                schema(),
                TableStatistics::with_row_count(left_rows),
            )
            .expect("a"),
            TableEntry::new(
                TableId::new(32),
                "b",
                schema(),
                TableStatistics::with_row_count(right_rows),
            )
            .expect("b"),
        ],
    )
    .expect("database")])
    .expect("catalog");
    let provider = SnapshotScanProvider::new([
        (database_id, TableId::new(31), &a_snapshot),
        (database_id, TableId::new(32), &b_snapshot),
    ])
    .expect("provider");

    let cases: &[(&str, &str)] = &[
        // Target 1: dependent subqueries.
        (
            "t1_in_join",
            "SELECT COUNT(*) FROM a WHERE a.k IN (SELECT DISTINCT i.k FROM b i JOIN b j ON \
             j.id = i.id AND i.k = a.k WHERE j.x > 5)",
        ),
        (
            "t1_in_join_perrow",
            "SELECT COUNT(*) FROM a WHERE a.k IN (SELECT DISTINCT i.k FROM b i JOIN b j ON \
             j.id = i.id AND i.x = a.x WHERE j.x > 5)",
        ),
        (
            "t1_in_join_plain",
            "SELECT COUNT(*) FROM a WHERE a.k IN (SELECT i.k FROM b i JOIN b j ON \
             j.id = i.id AND i.k = a.k WHERE j.x > 5)",
        ),
        (
            "t1_in_join_plain_perrow",
            "SELECT COUNT(*) FROM a WHERE a.k IN (SELECT i.k FROM b i JOIN b j ON \
             j.id = i.id AND i.x = a.x WHERE j.x > 5)",
        ),
        (
            "t1_in_distinct_where",
            "SELECT COUNT(*) FROM a WHERE a.k IN (SELECT DISTINCT i.k FROM b i JOIN b j ON \
             j.id = i.id WHERE i.x = a.x AND j.x > 5)",
        ),
        (
            "t1_in_distinct_single",
            "SELECT COUNT(*) FROM a WHERE a.k IN (SELECT DISTINCT i.k FROM b i WHERE i.x = a.x)",
        ),
        (
            "t1_exists_distinct",
            "SELECT COUNT(*) FROM a WHERE EXISTS (SELECT DISTINCT i.k FROM b i JOIN b j ON \
             j.id = i.id WHERE i.x = a.x AND j.x > 5)",
        ),
        (
            "t1_scalar_limit",
            "SELECT SUM((SELECT i.x FROM b i WHERE i.k = a.k ORDER BY i.x DESC, i.id LIMIT 1)) \
             FROM a",
        ),
        (
            "t1_scalar_limit_perrow",
            "SELECT SUM((SELECT i.x FROM b i WHERE i.x + 0 = a.x ORDER BY i.x DESC, i.id LIMIT \
             1)) FROM a",
        ),
        (
            "t1_exists_limit_perrow",
            "SELECT COUNT(*) FROM a WHERE EXISTS (SELECT 1 FROM b i WHERE i.x + 0 = a.x AND \
             i.k >= 0 LIMIT 1)",
        ),
        (
            "t1_nested_scalar",
            "SELECT SUM((SELECT (SELECT MAX(j.x) FROM b j WHERE j.k = i.k) FROM b i WHERE i.id = \
             a.id)) FROM a",
        ),
        (
            "t1_nested_first_row",
            "SELECT SUM((SELECT (SELECT MAX(j.x) FROM b j WHERE j.k = i.k) FROM b i WHERE i.x = \
             a.x ORDER BY i.id LIMIT 1)) FROM a",
        ),
        (
            "t1_nested_in_where",
            "SELECT COUNT(*) FROM a WHERE a.x = (SELECT i.x FROM b i WHERE i.id = a.id AND i.x \
             <= (SELECT MAX(j.x) FROM b j WHERE j.k = i.k))",
        ),
        (
            "t1_having",
            "SELECT SUM((SELECT SUM(i.x) FROM b i WHERE i.x = a.x HAVING COUNT(*) > 0)) FROM a",
        ),
        // Target 2: a subquery in an outer join's ON reaching the preserved side.
        (
            "t2_on_in",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND a.x IN (SELECT \
             c.x FROM b c WHERE c.k = a.k)",
        ),
        (
            "t2_on_not_in",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND a.x NOT IN \
             (SELECT c.x FROM b c WHERE c.k = a.k)",
        ),
        (
            "t2_on_exists",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND EXISTS (SELECT 1 \
             FROM b c WHERE c.k = a.k AND c.x = a.x)",
        ),
        (
            "t2_on_not_exists",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND NOT EXISTS \
             (SELECT 1 FROM b c WHERE c.k = a.k AND c.x = a.x)",
        ),
        (
            "t2_on_exists_ineq",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND EXISTS (SELECT 1 \
             FROM b c WHERE c.k = a.k AND c.x > a.x)",
        ),
        (
            "t2_on_exists_join",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND EXISTS (SELECT 1 \
             FROM b c JOIN b d ON d.id = c.id WHERE c.x = a.x)",
        ),
        (
            "t2_nokey_not_exists",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.x < a.x - 3 AND b.x > a.x - 6 \
             AND NOT EXISTS (SELECT 1 FROM b c WHERE c.x = a.x AND c.k = 1)",
        ),
        (
            "t2_on_both_sides",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND EXISTS (SELECT 1 \
             FROM b c WHERE c.x = a.x AND c.k = b.k)",
        ),
        (
            "t2b_not_in",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND b.x NOT IN \
             (SELECT c.x FROM b c WHERE c.lo = a.x)",
        ),
        (
            "t2b_not_exists",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND NOT EXISTS \
             (SELECT 1 FROM b c WHERE c.x = b.x AND c.lo = a.x)",
        ),
        (
            "t2b_exists_ineq",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND EXISTS (SELECT 1 \
             FROM b c WHERE c.x = b.x AND c.lo < a.x)",
        ),
        (
            "t2b_in_join",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND b.x IN (SELECT \
             c.x FROM b c JOIN b d ON d.id = c.id WHERE d.lo = a.x)",
        ),
        (
            "t2b_in_grouped",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND b.x IN (SELECT \
             c.x FROM b c WHERE c.lo = a.x GROUP BY c.x HAVING COUNT(*) >= 1)",
        ),
        (
            "t2b_in_plain",
            "SELECT COUNT(*), COUNT(b.id) FROM a LEFT JOIN b ON b.k = a.k AND b.x IN (SELECT \
             c.x FROM b c WHERE c.lo = a.x)",
        ),
        // Target 3: joins with no hashable key.
        (
            "t3_int_band",
            "SELECT COUNT(*), SUM(b.id) FROM a JOIN b ON b.x BETWEEN a.x AND a.x + 2",
        ),
        (
            "t3_decimal_band",
            "SELECT COUNT(*), SUM(b.id) FROM a JOIN b ON b.amt BETWEEN a.amt AND a.amt + 2",
        ),
        (
            "t3_decimal_plain",
            "SELECT COUNT(*), SUM(b.id) FROM a JOIN b ON b.amt < a.amt AND b.x > a.x - 3",
        ),
        (
            "t3_datetime_interval",
            "SELECT COUNT(*), SUM(b.id) FROM a JOIN b ON b.t0 >= a.t0 AND b.t0 < a.t0 + INTERVAL \
             3 MINUTE",
        ),
        (
            "t3_datetime_mixed",
            "SELECT COUNT(*), SUM(b.id) FROM a JOIN b ON b.t3 >= a.t0 AND b.t3 < a.t0 + INTERVAL \
             3 MINUTE",
        ),
        (
            "t3_datetime_mixed_plain",
            "SELECT COUNT(*), SUM(b.id) FROM a JOIN b ON b.t3 > a.t0 AND b.x < a.x + 3",
        ),
        (
            "t3_text",
            "SELECT COUNT(*), SUM(b.id) FROM a JOIN b ON b.s >= a.s AND b.s <= a.s2",
        ),
        (
            "t3_contains",
            "SELECT COUNT(*), SUM(b.id) FROM a JOIN b ON a.x BETWEEN b.lo AND b.hi",
        ),
        (
            "t3_contains_left",
            "SELECT COUNT(*), SUM(b.id) FROM a LEFT JOIN b ON a.x BETWEEN b.lo AND b.hi",
        ),
        (
            "t3_contains_count",
            "SELECT COUNT(*) FROM a JOIN b ON a.x BETWEEN b.lo AND b.hi",
        ),
    ];
    println!();
    println!("left {left_rows} right {right_rows} keys {keys}");
    println!("| case | ms | inner executions | pairs tested | answer | declines |");
    println!("|---|---:|---:|---:|---|---|");
    for (name, sql) in cases {
        if !only.is_empty() && !only.split(',').any(|prefix| name.starts_with(prefix)) {
            continue;
        }
        let _ = take_dependent_declines();
        let _ = take_exec_counters();
        let statement = parse_statement(sql).expect("parse");
        let bound = match Binder::new(&catalog, Some("app")).bind(&statement) {
            Ok(bound) => bound,
            Err(error) => {
                println!("| {name} | - | - | bind: {error} | |");
                continue;
            }
        };
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let before = dependent_subquery_executions();
        let started = Instant::now();
        let mut answer = Vec::new();
        let outcome = (|| {
            let mut execution = Execution::start(
                physical,
                &provider,
                2 * 1024 * 1024 * 1024,
                Collation::default(),
            )?;
            while let Some(batch) = execution.next_batch()? {
                for row in batch.selection().selected_rows() {
                    answer.push(
                        (0..batch.columns().len())
                            .map(|column| {
                                format!(
                                    "{:?}",
                                    batch.column(column).and_then(|values| values.value(row))
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                }
            }
            Ok::<_, pintail_exec::ExecError>(())
        })();
        let ms = started.elapsed().as_secs_f64() * 1_000.0;
        let mut declines = take_dependent_declines();
        declines.sort();
        declines.dedup();
        outcome.unwrap_or_else(|error| panic!("{name}: {error}"));
        let counters = take_exec_counters();
        println!(
            "| {name} | {ms:.1} | {} | {} | {} | {} |",
            dependent_subquery_executions() - before,
            counters.range_join_pairs_tested,
            answer.first().expect("one row of aggregates"),
            declines.join(" ; ")
        );
    }
}

/// What one dependent execution costs to plan and what it costs to run:
/// each inner query, with a literal where the outer value goes, planned
/// and executed from its bound form as the dependent path does.
#[test]
fn plan_share() {
    let right_rows = env("SURVEY_RIGHT", 300);
    let keys = env("SURVEY_KEYS", 50);
    let rounds = env("SURVEY_ROUNDS", 200);
    let b_dir = tempfile::tempdir().expect("dir");
    let mut b = TableStore::open(b_dir.path(), schema(), StoreOptions::default()).expect("open");
    b.bulk_ingest_snapshot(rows(right_rows, keys)).expect("b");
    let b_snapshot = b.snapshot();
    let database_id = DatabaseId::new(3);
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        database_id,
        "app",
        [TableEntry::new(
            TableId::new(32),
            "b",
            schema(),
            TableStatistics::with_row_count(right_rows),
        )
        .expect("b")],
    )
    .expect("database")])
    .expect("catalog");
    let provider = SnapshotScanProvider::new([(database_id, TableId::new(32), &b_snapshot)])
        .expect("provider");
    let cases = [
        (
            "in_join",
            "SELECT DISTINCT i.k FROM b i JOIN b j ON j.id = i.id AND i.x = 17 WHERE j.x > 5",
        ),
        (
            "scalar_limit",
            "SELECT i.x FROM b i WHERE i.x + 0 = 17 ORDER BY i.x DESC, i.id LIMIT 1",
        ),
        (
            "exists_limit",
            "SELECT 1 FROM b i WHERE i.x + 0 = 17 AND i.k >= 0 LIMIT 1",
        ),
        (
            "having",
            "SELECT SUM(i.x) FROM b i WHERE i.x = 17 HAVING COUNT(*) > 0",
        ),
    ];
    println!();
    println!("inner table {right_rows} rows, {rounds} rounds");
    println!("| inner query | plan us | start us | run us | plan share |");
    println!("|---|---:|---:|---:|---:|");
    for (name, sql) in cases {
        let statement = parse_statement(sql).expect("parse");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind");
        let (mut plan, mut start, mut run) = (0.0_f64, 0.0_f64, 0.0_f64);
        for _ in 0..rounds {
            let at = Instant::now();
            let query = bound.clone();
            let physical = PhysicalPlanner::plan(
                Optimizer::optimize(LogicalPlanner::plan(query)),
                Collation::default(),
            )
            .expect("plan");
            plan += at.elapsed().as_secs_f64();
            let at = Instant::now();
            let mut execution =
                Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
                    .expect("start");
            start += at.elapsed().as_secs_f64();
            let at = Instant::now();
            while let Some(batch) = execution.next_batch().expect("batch") {
                std::hint::black_box(batch.row_count());
            }
            drop(execution);
            run += at.elapsed().as_secs_f64();
        }
        #[allow(clippy::cast_precision_loss)]
        let per = 1e6 / rounds as f64;
        println!(
            "| {name} | {:.1} | {:.1} | {:.1} | {:.1}% |",
            plan * per,
            start * per,
            run * per,
            plan / (plan + start + run) * 100.0
        );
    }
}
