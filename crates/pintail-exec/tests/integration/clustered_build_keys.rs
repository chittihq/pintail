//! A child table whose rows arrive grouped by parent, joined to a few
//! parents. The join reads the parents first and filters the children's
//! build by their keys; the scan also tests those keys on the join column
//! before decoding the child's other columns, so the rows of the few
//! parents are the only ones decoded in full. The answers must be the ones
//! the unfiltered join gives, for inner and left joins and a key the
//! children never carry.
//!
//! The measurement is `#[ignore]`d:
//! `cargo test --profile recovery -p pintail-exec --test integration clustered_build_keys::
//! -- --ignored --nocapture`.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const PARENT_ID: TableId = TableId::new(1);
const CHILD_ID: TableId = TableId::new(2);
const PARENTS: u64 = 1_000;
const CHILDREN: u64 = 400_000;
const PER_PARENT: u64 = CHILDREN / PARENTS;

fn parent_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
        ],
    )
    .expect("parent schema")
}

fn child_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "parent_id", DataType::UInt64, true),
            Column::new(3, "amount", DataType::Int64, false),
            Column::new(4, "note", DataType::Utf8, false),
        ],
    )
    .expect("child schema")
}

/// Children arrive in parent order; every hundredth has no parent.
fn child_parent(id: u64) -> Option<u64> {
    (!id.is_multiple_of(100)).then(|| (id - 1) / PER_PARENT + 1)
}

fn child_amount(id: u64) -> i64 {
    i64::try_from(id % 89).expect("amount") - 30
}

struct Fixture {
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    parent: TableStore,
    child: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new() -> Self {
        let parent_dir = tempfile::tempdir().expect("parent dir");
        let child_dir = tempfile::tempdir().expect("child dir");
        let mut parent =
            TableStore::open(parent_dir.path(), parent_schema(), StoreOptions::default())
                .expect("open parent");
        parent
            .bulk_ingest_snapshot(
                (1..=PARENTS)
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            vec![Value::UInt64(id), Value::Utf8(format!("parent-{id:04}"))],
                            id,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("parents");
        let mut child = TableStore::open(child_dir.path(), child_schema(), StoreOptions::default())
            .expect("open child");
        child
            .bulk_ingest_snapshot(
                (1..=CHILDREN)
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            vec![
                                Value::UInt64(id),
                                child_parent(id).map_or(Value::Null, Value::UInt64),
                                Value::Int64(child_amount(id)),
                                Value::Utf8(format!(
                                    "child-{id:07}-with-a-note-wide-enough-to-cost-a-decode"
                                )),
                            ],
                            id,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("children");
        let parent_entry = TableEntry::new(
            PARENT_ID,
            "parent",
            parent_schema(),
            TableStatistics::with_row_count(PARENTS),
        )
        .expect("parent entry")
        .with_key_columns([1])
        .expect("parent key");
        let child_entry = TableEntry::new(
            CHILD_ID,
            "child",
            child_schema(),
            TableStatistics::with_row_count(CHILDREN),
        )
        .expect("child entry")
        .with_key_columns([1])
        .expect("child key");
        let database =
            DatabaseEntry::new(DATABASE_ID, "app", [parent_entry, child_entry]).expect("database");
        Self {
            _dirs: (parent_dir, child_dir),
            parent,
            child,
            catalog: CatalogSnapshot::new([database]).expect("catalog"),
        }
    }

    fn run(&self, sql: &str) -> Vec<Vec<Value>> {
        let parent = self.parent.snapshot();
        let child = self.child.snapshot();
        let provider = SnapshotScanProvider::new([
            (DATABASE_ID, PARENT_ID, &parent),
            (DATABASE_ID, CHILD_ID, &child),
        ])
        .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 1 << 30, Collation::default()).expect("start");
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
                        .collect(),
                );
            }
        }
        rows
    }
}

const PICKED: [u64; 3] = [7, 500, 993];

const LEFT_JOIN: &str = "SELECT COUNT(*), COUNT(c.id), SUM(c.amount), MAX(c.note) \
     FROM parent p LEFT JOIN child c ON c.parent_id = p.id \
     WHERE p.id IN (7, 500, 993, 1000000)";

fn expected(parents: &[u64]) -> (u64, i64, String) {
    let mut count = 0;
    let mut sum = 0;
    let mut note = String::new();
    for id in 1..=CHILDREN {
        if child_parent(id).is_some_and(|parent| parents.contains(&parent)) {
            count += 1;
            sum += child_amount(id);
            note = note.max(format!(
                "child-{id:07}-with-a-note-wide-enough-to-cost-a-decode"
            ));
        }
    }
    (count, sum, note)
}

#[test]
fn a_few_parents_join_exactly_the_children_they_have() {
    let fixture = Fixture::new();
    let (count, sum, note) = expected(&PICKED);
    assert_eq!(
        fixture.run(LEFT_JOIN),
        vec![vec![
            Value::UInt64(count),
            Value::UInt64(count),
            Value::Utf8(sum.to_string()),
            Value::Utf8(note.clone())
        ]]
    );
    assert_eq!(
        fixture.run(
            "SELECT COUNT(*), SUM(c.amount) FROM parent p JOIN child c ON c.parent_id = p.id \
             WHERE p.id IN (7, 500, 993)"
        ),
        vec![vec![Value::UInt64(count), Value::Utf8(sum.to_string())]]
    );
    // The child scan's own tests, left to the Filters above once the join's
    // keys have chosen a few rows, still decide which of those rows stay.
    let kept = (1..=CHILDREN)
        .filter(|id| {
            child_parent(*id).is_some_and(|parent| PICKED.contains(&parent))
                && child_amount(*id) > 10
                && id.to_string().contains('7')
        })
        .collect::<Vec<_>>();
    let kept_sum: i64 = kept.iter().map(|id| child_amount(*id)).sum();
    assert_eq!(
        fixture.run(
            "SELECT COUNT(*), COUNT(c.id), SUM(c.amount) FROM parent p LEFT JOIN child c \
             ON c.parent_id = p.id AND c.amount > 10 AND c.note LIKE 'child-%7%-with%' \
             WHERE p.id IN (7, 500, 993)"
        ),
        vec![vec![
            Value::UInt64(kept.len() as u64),
            Value::UInt64(kept.len() as u64),
            Value::Utf8(kept_sum.to_string())
        ]]
    );
    // A parent with no children keeps its row in a left join, and joins
    // nothing in an inner one.
    let childless = fixture.run(
        "SELECT COUNT(*), COUNT(c.id) FROM parent p LEFT JOIN child c \
         ON c.parent_id = p.id + 5000 WHERE p.id IN (7, 500)",
    );
    assert_eq!(childless, vec![vec![Value::UInt64(2), Value::UInt64(0)]]);
}

#[test]
#[ignore = "measurement"]
fn measure_a_few_parents_joined_to_their_children() {
    let fixture = Fixture::new();
    for _ in 0..5 {
        let started = std::time::Instant::now();
        let _ = fixture.run(LEFT_JOIN);
        eprintln!(
            "few parents, clustered children: {:.1} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
}
