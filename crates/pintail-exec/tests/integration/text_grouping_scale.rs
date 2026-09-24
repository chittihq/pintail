//! Grouping on a text key, or on several keys, finds a row's group through
//! a hash of its raw bytes; a miss used to fall to a scan of every group so
//! far, comparing under the key's collation. Every new group is a miss, so
//! the scan made the work quadratic in the group count. A second index,
//! keyed on the collation's own key bytes, answers the miss instead.
//!
//! That scan's comparison also took an ASCII case-insensitive shortcut for
//! every collation, so under a binary collation `'a'` found the group of
//! `'A'` and the two were counted as one.
//!
//! The measurement is `#[ignore]`d:
//! `cargo test --profile recovery -p pintail-exec --test text_grouping_scale
//! -- --ignored --nocapture`.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "name", DataType::Utf8, false)
                .with_collation(Some("utf8mb4_0900_ai_ci".to_owned())),
            Column::new(3, "code", DataType::Utf8, false)
                .with_collation(Some("utf8mb4_bin".to_owned())),
            Column::new(4, "grp", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new(rows: impl IntoIterator<Item = (String, String, i64)>) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut table =
            TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
        let rows = rows
            .into_iter()
            .zip(0_u64..)
            .map(|((name, code, grp), id)| {
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                    vec![
                        Value::UInt64(id),
                        Value::Utf8(name),
                        Value::Utf8(code),
                        Value::Int64(grp),
                    ],
                    id + 1,
                    false,
                )
            })
            .collect::<Vec<_>>();
        let count = u64::try_from(rows.len()).expect("row count");
        table.bulk_ingest_snapshot(rows).expect("rows");
        let entry = TableEntry::new(
            TableId::new(1),
            "items",
            schema(),
            TableStatistics::with_row_count(count),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        Self {
            _directory: directory,
            table,
            catalog: CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
            ])
            .expect("catalog"),
        }
    }

    fn run(&self, sql: &str) -> (Vec<Vec<String>>, f64) {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let started = std::time::Instant::now();
        let mut execution =
            Execution::start(physical, &provider, 1 << 31, Collation::default()).expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                rows.push(
                    (0..batch.columns().len())
                        .map(|column| {
                            let value = batch
                                .column(column)
                                .and_then(|column| column.value(row))
                                .cloned()
                                .expect("selected value");
                            value
                                .text()
                                .map_or_else(|| format!("{value:?}"), str::to_owned)
                        })
                        .collect::<Vec<_>>(),
                );
            }
        }
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        rows.sort();
        (rows, elapsed)
    }
}

#[test]
fn a_binary_collation_keeps_case_apart_and_a_folding_one_merges_it() {
    let spellings = ["a", "A", "b", "B", "a", "B"];
    let fixture = Fixture::new(
        spellings
            .iter()
            .map(|text| ((*text).to_owned(), (*text).to_owned(), 1)),
    );
    let (binary, _) = fixture.run("SELECT code, COUNT(*) FROM items GROUP BY code");
    assert_eq!(
        binary,
        vec![
            vec!["A".to_owned(), "UInt64(1)".to_owned()],
            vec!["B".to_owned(), "UInt64(2)".to_owned()],
            vec!["a".to_owned(), "UInt64(2)".to_owned()],
            vec!["b".to_owned(), "UInt64(1)".to_owned()],
        ]
    );
    let (folded, _) = fixture.run("SELECT COUNT(*) FROM (SELECT name FROM items GROUP BY name) t");
    assert_eq!(folded, vec![vec!["UInt64(2)".to_owned()]]);
    let (pairs, _) =
        fixture.run("SELECT COUNT(*) FROM (SELECT code, grp FROM items GROUP BY code, grp) t");
    assert_eq!(pairs, vec![vec!["UInt64(4)".to_owned()]]);
}

#[test]
fn accent_and_case_variants_share_a_group_across_many_groups() {
    // Enough groups that a variant spelling arrives long after its group was
    // made, among thousands of others.
    let fixture = Fixture::new((0..40_000_u32).map(|id| {
        let base = format!("name-{:05}", id % 5_000);
        let name = if id % 2 == 0 {
            base
        } else {
            base.to_uppercase()
        };
        (name, format!("c{}", id % 3), i64::from(id % 7))
    }));
    let (groups, _) = fixture.run("SELECT COUNT(*) FROM (SELECT name FROM items GROUP BY name) t");
    assert_eq!(groups, vec![vec!["UInt64(5000)".to_owned()]]);
    let (pairs, _) =
        fixture.run("SELECT COUNT(*) FROM (SELECT name, grp FROM items GROUP BY name, grp) t");
    // name-N appears with grp values id % 7 for ids congruent to N mod 5000;
    // 5000 and 7 are coprime, so each name meets all seven.
    assert_eq!(pairs, vec![vec!["UInt64(35000)".to_owned()]]);
}

#[test]
#[ignore = "measurement, not an assertion"]
fn text_grouping_cost_by_group_count() {
    for groups in [1_000_u32, 20_000, 100_000] {
        let fixture = Fixture::new((0..2_000_000_u32).map(|id| {
            (
                format!("name-{:06}", id % groups),
                format!("c{}", id % groups),
                i64::from(id % 7),
            )
        }));
        for sql in [
            "SELECT name, COUNT(*) FROM items GROUP BY name",
            "SELECT code, grp, COUNT(*) FROM items GROUP BY code, grp",
        ] {
            let mut timings = (0..3).map(|_| fixture.run(sql).1).collect::<Vec<_>>();
            timings.sort_by(f64::total_cmp);
            println!(
                "[groups {groups:>6}] median {:>9.1} ms  min {:>9.1} ms  {sql}",
                timings[1], timings[0]
            );
        }
    }
}
