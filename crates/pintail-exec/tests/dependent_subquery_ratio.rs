//! What repeated outer keys cost a correlated subquery.
//!
//! The dependent path resolves a correlated subquery once per outer row:
//! it clones the inner query, substitutes that row's outer values as
//! literals, and plans and executes it from scratch. When many outer rows
//! carry the same correlation values - a million orders over ten thousand
//! customers - the same inner question is answered again and again.
//!
//! This is the measurement that decides whether that is worth fixing. An
//! outer table of N rows carries D distinct correlation keys; the sweep
//! varies N/D from 1 (every key distinct, nothing to share) to N (one key,
//! everything shareable) while N stays fixed, so any change in cost is the
//! repetition and nothing else. Inner executions are counted, not inferred:
//! the counter proves each shape took the dependent path (a decorrelated
//! shape would run zero) and ran exactly once per outer row.
//!
//! Sizes default small so the correctness assertions run in the unit gate.
//! `PINTAIL_RATIO_ROWS=20000 cargo test --test dependent_subquery_ratio -- --nocapture`
//! prints the table at a size where the numbers mean something.

use std::time::Instant;

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

/// Inner rows per key, and how many keys the inner table has. Fixed across
/// the sweep so the inner work per execution is constant.
const INNER_ROWS_PER_KEY: u64 = 8;
const INNER_KEYS: u64 = 1_000;

fn outer_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "k", DataType::UInt64, false),
        ],
    )
    .expect("outer schema")
}

fn inner_schema() -> TableSchema {
    TableSchema::new(
        2,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "k", DataType::UInt64, false),
            Column::new(3, "x", DataType::UInt64, false),
        ],
    )
    .expect("inner schema")
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
    _outer_dir: tempfile::TempDir,
    _inner_dir: tempfile::TempDir,
    outer: TableStore,
    inner: TableStore,
    outer_rows: u64,
}

/// `outer` has `rows` rows whose key is `id % distinct`; `inner` has
/// `INNER_KEYS * INNER_ROWS_PER_KEY` rows with keys spread over `INNER_KEYS`.
fn fixture(rows: u64, distinct: u64) -> Fixture {
    let outer_dir = tempfile::tempdir().expect("outer dir");
    let inner_dir = tempfile::tempdir().expect("inner dir");
    let mut outer =
        TableStore::open(outer_dir.path(), outer_schema(), StoreOptions::default()).expect("outer");
    outer
        .bulk_ingest_snapshot(
            (1..=rows)
                .map(|id| stored(id, vec![Value::UInt64(id), Value::UInt64(id % distinct)]))
                .collect(),
        )
        .expect("outer snapshot");
    let mut inner =
        TableStore::open(inner_dir.path(), inner_schema(), StoreOptions::default()).expect("inner");
    inner
        .bulk_ingest_snapshot(
            (1..=INNER_KEYS * INNER_ROWS_PER_KEY)
                .map(|id| {
                    stored(
                        id,
                        vec![
                            Value::UInt64(id),
                            Value::UInt64(id % INNER_KEYS),
                            Value::UInt64(id % 17),
                        ],
                    )
                })
                .collect(),
        )
        .expect("inner snapshot");
    Fixture {
        _outer_dir: outer_dir,
        _inner_dir: inner_dir,
        outer,
        inner,
        outer_rows: rows,
    }
}

/// Runs one statement and returns its row count and the number of inner
/// executions it took.
fn measure(fixture: &Fixture, sql: &str) -> (usize, u64, f64) {
    let outer_snapshot = fixture.outer.snapshot();
    let inner_snapshot = fixture.inner.snapshot();
    let database_id = DatabaseId::new(3);
    let outer_id = TableId::new(31);
    let inner_id = TableId::new(32);
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [
            TableEntry::new(
                outer_id,
                "outer_t",
                outer_schema(),
                TableStatistics::with_row_count(fixture.outer_rows),
            )
            .expect("outer entry"),
            TableEntry::new(
                inner_id,
                "inner_t",
                inner_schema(),
                TableStatistics::with_row_count(INNER_KEYS * INNER_ROWS_PER_KEY),
            )
            .expect("inner entry"),
        ],
    )
    .expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider = SnapshotScanProvider::new([
        (database_id, outer_id, &outer_snapshot),
        (database_id, inner_id, &inner_snapshot),
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
    let started = Instant::now();
    let mut execution =
        Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
            .expect("start");
    let mut rows = 0;
    while let Some(batch) = execution.next_batch().expect("batch") {
        rows += batch.selection().selected_rows().count();
    }
    let elapsed = started.elapsed().as_secs_f64() * 1_000.0;
    (rows, dependent_subquery_executions() - before, elapsed)
}

/// The three correlated shapes, each written so it cannot decorrelate into
/// a join: the scalar and EXISTS carry a LIMIT, the IN joins two inner
/// relations. A shape the optimizer rewrote would report zero inner
/// executions and the counter assertion would say so.
const SHAPES: [(&str, &str); 3] = [
    (
        "scalar",
        "SELECT o.id, (SELECT i.x FROM inner_t i WHERE i.k = o.k ORDER BY i.x DESC, i.id LIMIT 1) \
         AS top FROM outer_t o",
    ),
    (
        "exists",
        "SELECT o.id FROM outer_t o WHERE EXISTS (SELECT 1 FROM inner_t i WHERE i.k = o.k AND \
         i.x > 5 LIMIT 1)",
    ),
    (
        "in",
        "SELECT o.id FROM outer_t o WHERE o.k IN (SELECT i.k FROM inner_t i JOIN inner_t j ON \
         j.id = i.id WHERE i.k = o.k AND j.x > 5)",
    ),
];

#[test]
// The per-row figure is a printed rate; a 52-bit mantissa loses nothing at
// any row count this test will ever be given.
#[allow(clippy::cast_precision_loss)]
fn every_correlated_shape_executes_its_inner_query_once_per_outer_row() {
    let rows: u64 = std::env::var("PINTAIL_RATIO_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    // Distinct outer keys D, never above the inner key space: every outer
    // key must match the same INNER_ROWS_PER_KEY inner rows in every
    // column, or the sweep measures match fraction instead of repetition.
    // (A first version let D exceed the inner keys, and the "all distinct"
    // column was mostly rows whose inner query found nothing - cheaper for
    // an unrelated reason.)
    let distinct_keys: [u64; 4] = [INNER_KEYS, INNER_KEYS / 10, INNER_KEYS / 100, 1];
    println!();
    println!(
        "outer rows N = {rows}; inner table {} rows over {INNER_KEYS} keys; every outer key \
         matches {INNER_ROWS_PER_KEY} inner rows",
        INNER_KEYS * INNER_ROWS_PER_KEY
    );
    println!(
        "| shape | distinct keys D | repeats per key N/D | inner executions | ms | ms per outer row |"
    );
    println!("|---|---:|---:|---:|---:|---:|");
    for (name, sql) in SHAPES {
        let mut per_ratio = Vec::new();
        for distinct in distinct_keys {
            let distinct = distinct.min(rows).max(1);
            let ratio = rows / distinct;
            let fixture = fixture(rows, distinct);
            let (result_rows, executions, ms) = measure(&fixture, sql);
            assert!(result_rows > 0, "{name}: the query answered nothing");
            assert_eq!(
                executions, rows,
                "{name}: the dependent path runs the inner query exactly once per outer row - \
                 {executions} executions for {rows} rows means the shape did not take it, or \
                 shared work it does not share today"
            );
            println!(
                "| {name} | {distinct} | {ratio} | {executions} | {ms:.0} | {:.3} |",
                ms / rows as f64
            );
            per_ratio.push(ms);
        }
        // The claim under test: cost is flat in the repetition because
        // nothing is shared. If a later change shares work, this is where
        // the single-key column must fall well below the least-repeated one.
        let least_repeated = per_ratio[0];
        let one_key = per_ratio[3];
        println!(
            "| {name} | — | — | — | — | single-key cost is {:.0}% of the least-repeated column |",
            one_key / least_repeated * 100.0
        );
    }
}
