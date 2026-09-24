//! A join whose probe side is small reads it first and filters the build
//! side by its keys, so a large table joined to a handful of rows is built
//! from the rows that can match rather than from the whole table. The
//! answer must not change for any join kind, the filter must not apply when
//! the probe side is too large to read ahead, and the build it leaves
//! behind must be small enough to fit where the whole table would not.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::spill::QuerySpillMetrics;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const DIM_ID: TableId = TableId::new(1);
const FACT_ID: TableId = TableId::new(2);
const DIMS: u64 = 2_000;
const FACTS: u64 = 200_000;
/// Dimension ids the facts reference; the last thousand facts point past
/// the dimension table and match nothing.
const REFERENCED: u64 = DIMS + 1_000;

fn dim_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "grp", DataType::Int64, false),
            Column::new(3, "name", DataType::Utf8, false),
        ],
    )
    .expect("dim schema")
}

fn fact_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "dim_id", DataType::UInt64, true),
            Column::new(3, "amount", DataType::Int64, false),
            Column::new(4, "note", DataType::Utf8, false),
        ],
    )
    .expect("fact schema")
}

fn dim_row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(i64::try_from(id % 40).expect("grp")),
            Value::Utf8(format!("dim-{id:05}")),
        ],
        id,
        false,
    )
}

/// The dimension a fact points at: `None` for the NULL keys.
fn fact_dim(id: u64) -> Option<u64> {
    (!id.is_multiple_of(101)).then(|| id % REFERENCED + 1)
}

fn fact_amount(id: u64) -> i64 {
    i64::try_from(id % 97).expect("amount") - 20
}

fn fact_row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            fact_dim(id).map_or(Value::Null, Value::UInt64),
            Value::Int64(fact_amount(id)),
            Value::Utf8(format!(
                "fact-{id:07}-with-a-note-wide-enough-to-cost-memory"
            )),
        ],
        id,
        false,
    )
}

struct Fixture {
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    dim: TableStore,
    fact: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new() -> Self {
        let dim_dir = tempfile::tempdir().expect("dim dir");
        let fact_dir = tempfile::tempdir().expect("fact dir");
        let mut dim = TableStore::open(dim_dir.path(), dim_schema(), StoreOptions::default())
            .expect("open dim");
        dim.ingest((1..=DIMS).map(dim_row).collect())
            .expect("ingest dim");
        let mut fact = TableStore::open(fact_dir.path(), fact_schema(), StoreOptions::default())
            .expect("open fact");
        fact.ingest((1..=FACTS).map(fact_row).collect())
            .expect("ingest fact");
        let dim_entry = TableEntry::new(
            DIM_ID,
            "dim",
            dim_schema(),
            TableStatistics::with_row_count(DIMS),
        )
        .expect("dim entry")
        .with_key_columns([1])
        .expect("dim key");
        let fact_entry = TableEntry::new(
            FACT_ID,
            "fact",
            fact_schema(),
            TableStatistics::with_row_count(FACTS),
        )
        .expect("fact entry")
        .with_key_columns([1])
        .expect("fact key");
        let database =
            DatabaseEntry::new(DATABASE_ID, "app", [dim_entry, fact_entry]).expect("database");
        Self {
            _dirs: (dim_dir, fact_dir),
            dim,
            fact,
            catalog: CatalogSnapshot::new([database]).expect("catalog"),
        }
    }

    fn run(&self, sql: &str, memory_limit: usize) -> (Vec<Vec<Value>>, QuerySpillMetrics) {
        let dim_snapshot = self.dim.snapshot();
        let fact_snapshot = self.fact.snapshot();
        let provider = SnapshotScanProvider::new([
            (DATABASE_ID, DIM_ID, &dim_snapshot),
            (DATABASE_ID, FACT_ID, &fact_snapshot),
        ])
        .expect("provider");
        let statement = parse_statement(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&statement)
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
        let physical = PhysicalPlanner::plan(logical, Collation::default()).expect("plan");
        let mut execution =
            Execution::start(physical, &provider, memory_limit, Collation::default())
                .expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for index in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value(index).cloned().expect("value"))
                        .collect::<Vec<_>>(),
                );
            }
        }
        (rows, execution.spill_metrics())
    }
}

const ROOMY: usize = 512 * 1024 * 1024;
/// Enough for a build of the facts that match forty dimensions, not for a
/// build of every fact.
const TIGHT: usize = 24 * 1024 * 1024;

/// Facts pointing at dimensions of group 3, computed from the generators.
fn expected_group_3() -> (u64, i64, u64) {
    let mut matched_rows = 0;
    let mut sum = 0;
    let mut dims_with_facts = std::collections::BTreeSet::new();
    for fact in 1..=FACTS {
        if let Some(dim) = fact_dim(fact)
            && dim <= DIMS
            && dim % 40 == 3
        {
            matched_rows += 1;
            sum += fact_amount(fact);
            dims_with_facts.insert(dim);
        }
    }
    (matched_rows, sum, dims_with_facts.len() as u64)
}

#[test]
fn a_small_probe_side_filters_the_build_and_every_join_kind_answers_exactly() {
    let fixture = Fixture::new();
    let (matched_rows, sum, dims_with_facts) = expected_group_3();
    let dims_in_group = (1..=DIMS).filter(|id| id % 40 == 3).count() as u64;

    // LEFT JOIN: every group-3 dimension appears, matched or not.
    let (rows, metrics) = fixture.run(
        "SELECT COUNT(*), COUNT(f.id), SUM(f.amount) FROM dim d \
         LEFT JOIN fact f ON f.dim_id = d.id WHERE d.grp = 3",
        TIGHT,
    );
    let unmatched = dims_in_group - dims_with_facts;
    assert_eq!(
        rows,
        vec![vec![
            Value::UInt64(matched_rows + unmatched),
            Value::UInt64(matched_rows),
            Value::Int64(sum)
        ]]
    );
    assert_eq!(metrics.files, 0, "the filtered build fits without spilling");

    // INNER JOIN: statistics say the probe side is small, so it reads
    // ahead and filters the build too.
    let (rows, metrics) = fixture.run(
        "SELECT COUNT(*), SUM(f.amount) FROM dim d JOIN fact f ON f.dim_id = d.id \
         WHERE d.grp = 3",
        TIGHT,
    );
    assert_eq!(
        rows,
        vec![vec![Value::UInt64(matched_rows), Value::Int64(sum)]]
    );
    assert_eq!(metrics.files, 0);

    // Semi and anti joins keep or drop probe rows by whether a match exists.
    let (rows, _) = fixture.run(
        "SELECT COUNT(*) FROM dim d WHERE d.grp = 3 \
         AND EXISTS (SELECT 1 FROM fact f WHERE f.dim_id = d.id)",
        TIGHT,
    );
    assert_eq!(rows, vec![vec![Value::UInt64(dims_with_facts)]]);
    let (rows, _) = fixture.run(
        "SELECT COUNT(*) FROM dim d WHERE d.grp = 3 \
         AND NOT EXISTS (SELECT 1 FROM fact f WHERE f.dim_id = d.id)",
        TIGHT,
    );
    assert_eq!(rows, vec![vec![Value::UInt64(unmatched)]]);

    // A residual ON predicate and a probe-side NULL-free key still agree
    // with the plain arithmetic.
    let positive = (1..=FACTS)
        .filter(|fact| {
            fact_dim(*fact).is_some_and(|dim| dim <= DIMS && dim % 40 == 3)
                && fact_amount(*fact) > 50
        })
        .count() as u64;
    let (rows, _) = fixture.run(
        "SELECT COUNT(f.id) FROM dim d LEFT JOIN fact f ON f.dim_id = d.id AND f.amount > 50 \
         WHERE d.grp = 3",
        TIGHT,
    );
    assert_eq!(rows, vec![vec![Value::UInt64(positive)]]);
}

#[test]
fn a_probe_side_past_the_read_ahead_takes_the_whole_build() {
    let fixture = Fixture::new();
    // Facts on the left: two hundred thousand probe rows exceed the
    // read-ahead, so no key set exists and the build takes every
    // dimension. The answer is the same either way; the point is that the
    // key set was never applied to a partial probe.
    let (rows, _) = fixture.run(
        "SELECT COUNT(*), COUNT(d.id) FROM fact f LEFT JOIN dim d ON d.id = f.dim_id",
        ROOMY,
    );
    let matched = (1..=FACTS)
        .filter(|fact| fact_dim(*fact).is_some_and(|dim| dim <= DIMS))
        .count() as u64;
    assert_eq!(
        rows,
        vec![vec![Value::UInt64(FACTS), Value::UInt64(matched)]]
    );

    // And the control for the tight ceiling: with the dimensions on the
    // right the build is small; with every fact on the right and a probe
    // side too large to read ahead, the same ceiling must spill.
    let (rows, metrics) = fixture.run(
        "SELECT COUNT(*), SUM(f.amount) FROM dim d JOIN fact f ON f.dim_id = d.id",
        TIGHT,
    );
    let (all_matched, all_sum) = (1..=FACTS)
        .filter(|fact| fact_dim(*fact).is_some_and(|dim| dim <= DIMS))
        .fold((0_u64, 0_i64), |(count, sum), fact| {
            (count + 1, sum + fact_amount(fact))
        });
    assert_eq!(
        rows,
        vec![vec![Value::UInt64(all_matched), Value::Int64(all_sum)]]
    );
    // Every dimension is a small probe side, so the build is the facts
    // that reference one: two thirds of them, which this ceiling cannot
    // hold either. The contrast that matters is the group-3 build above,
    // which fits, against the unfiltered build below, which cannot.
    assert!(metrics.files > 0);
    let (rows, metrics) = fixture.run(
        "SELECT COUNT(*), SUM(f.amount) FROM fact f2 JOIN fact f ON f.id = f2.id",
        TIGHT,
    );
    assert_eq!(
        rows,
        vec![vec![
            Value::UInt64(FACTS),
            Value::Int64((1..=FACTS).map(fact_amount).sum())
        ]]
    );
    assert!(
        metrics.files > 0,
        "an unfiltered build of every fact must spill at this ceiling"
    );
}
