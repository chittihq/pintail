//! A GROUP BY over a chain of LEFT joins that fans each group out into the
//! product of several independent branches, folded by COUNT(DISTINCT), AVG
//! and a conditional SUM.
//!
//! The measurement is `#[ignore]`d:
//! `cargo test --profile recovery -p pintail-exec --test integration fanout_join_aggregates::
//! -- --ignored --nocapture`.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

type Definition = (&'static str, Vec<&'static str>, Vec<Vec<Value>>);

struct Fixture {
    _directory: tempfile::TempDir,
    stores: Vec<TableStore>,
    catalog: CatalogSnapshot,
}

fn number(value: u64) -> Value {
    Value::UInt64(value)
}

impl Fixture {
    fn new(definitions: Vec<Definition>) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut stores = Vec::new();
        let mut tables = Vec::new();
        for (index, (name, columns, rows)) in definitions.into_iter().enumerate() {
            let schema = TableSchema::new(
                1,
                columns
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        Column::new(
                            u32::try_from(i + 1).expect("column"),
                            *name,
                            DataType::UInt64,
                            i != 0,
                        )
                    })
                    .collect(),
            )
            .expect("schema");
            let count = u64::try_from(rows.len()).expect("rows");
            let mut store = TableStore::open(
                directory.path().join(name),
                schema.clone(),
                StoreOptions::default(),
            )
            .expect("store");
            store
                .bulk_ingest_snapshot(
                    rows.into_iter()
                        .enumerate()
                        .map(|(i, values)| {
                            let id = u64::try_from(i + 1).expect("id");
                            StoredRow::new(
                                PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                                values,
                                id,
                                false,
                            )
                        })
                        .collect(),
                )
                .expect("ingest");
            tables.push(
                TableEntry::new(
                    TableId::new(u64::try_from(index + 1).expect("table")),
                    name,
                    schema,
                    TableStatistics::with_row_count(count),
                )
                .expect("entry")
                .with_key_columns([1])
                .expect("key"),
            );
            stores.push(store);
        }
        let catalog = CatalogSnapshot::new([
            DatabaseEntry::new(DatabaseId::new(1), "app", tables).expect("database")
        ])
        .expect("catalog");
        Self {
            _directory: directory,
            stores,
            catalog,
        }
    }

    fn run(&self, sql: &str) -> (Vec<String>, f64, String) {
        let snapshots: Vec<_> = self.stores.iter().map(TableStore::snapshot).collect();
        let provider = SnapshotScanProvider::new(snapshots.iter().enumerate().map(|(i, s)| {
            (
                DatabaseId::new(1),
                TableId::new(u64::try_from(i + 1).expect("table")),
                s,
            )
        }))
        .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let started = std::time::Instant::now();
        let mut execution =
            Execution::start_profiled(plan, &provider, 1 << 31, None, Collation::default())
                .expect("execution");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                rows.push(
                    (0..batch.columns().len())
                        .map(|column| {
                            let value = batch
                                .column(column)
                                .and_then(|column| column.value_owned(row))
                                .expect("value");
                            value
                                .text()
                                .map_or_else(|| format!("{value:?}"), str::to_owned)
                        })
                        .collect::<Vec<_>>()
                        .join("|"),
                );
            }
        }
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        let profile = execution
            .profile()
            .map(|profile| profile.render())
            .unwrap_or_default();
        rows.sort();
        (rows, elapsed, profile)
    }
}

/// Members of a few groups, each with several independent child branches:
/// per-person entries, a plan of items with optional references, and
/// sessions per item with per-person checkins.
#[allow(clippy::too_many_lines)] // fixture data reads best inline
fn fixture(members_per_group: u64, entries: u64, items: u64, sessions: u64) -> Fixture {
    const GROUPS: u64 = 8;
    const PLANS: u64 = 4;
    let people = GROUPS * members_per_group;
    let members = (1..=people)
        .map(|id| {
            vec![
                number(id),
                number((id - 1) % GROUPS + 1),
                number(id),
                number(u64::from(id % 11 != 0)),
                if id % 3 == 0 {
                    number(1_000)
                } else {
                    Value::Null
                },
            ]
        })
        .collect();
    let persons = (1..=people)
        .map(|id| vec![number(id), number(id * 7 % 1_000)])
        .collect();
    // Every thirteenth person has no entries, so its groups meet none.
    let entry_rows = (1..=people * entries)
        .filter(|id| !((id - 1) % people + 1).is_multiple_of(13))
        .map(|id| {
            vec![
                number(id),
                number((id - 1) % people + 1),
                if id % 7 == 0 {
                    Value::Null
                } else {
                    number(id % 101)
                },
            ]
        })
        .collect();
    let groups = (1..=GROUPS)
        .map(|id| vec![number(id), number((id - 1) % PLANS + 1)])
        .collect();
    let item_rows: Vec<Vec<Value>> = (1..=PLANS * items)
        .map(|id| vec![number(id), number((id - 1) % PLANS + 1)])
        .collect();
    let clips = (1..=PLANS * items)
        .filter(|id| id % 3 != 0)
        .map(|id| vec![number(id), number(id), number(id % 50 + 1)])
        .collect();
    let refs = (1..=60)
        .map(|id| {
            vec![
                number(id),
                number(id % 55 + 1),
                number(u64::from(id % 4 != 0)),
            ]
        })
        .collect();
    let receipts = (1..=people * 3)
        .map(|id| vec![number(id), number(id % 51 + 1), number(id % people + 1)])
        .collect();
    let session_rows = (1..=PLANS * items * sessions)
        .map(|id| {
            vec![
                number(id),
                number((id - 1) % (PLANS * items) + 1),
                number(id % 2_000),
            ]
        })
        .collect();
    let checkins = (1..=PLANS * items * sessions)
        .flat_map(|session| {
            (0..3).map(move |k| {
                vec![
                    number(session * 3 + k),
                    number(session),
                    number((session * 13 + k * 101) % people + 1),
                    number(u64::from((session + k) % 4 != 0)),
                ]
            })
        })
        .collect();
    Fixture::new(vec![
        (
            "members",
            vec!["id", "grp", "person_id", "active", "since"],
            members,
        ),
        ("persons", vec!["id", "score"], persons),
        ("entries", vec!["id", "person_id", "progress"], entry_rows),
        ("grps", vec!["id", "plan_id"], groups),
        ("items", vec!["id", "plan_id"], item_rows),
        ("clips", vec!["id", "item_id", "ref_id"], clips),
        ("refs", vec!["id", "target_id", "live"], refs),
        ("receipts", vec!["id", "target_id", "person_id"], receipts),
        ("sessions", vec!["id", "item_id", "at"], session_rows),
        (
            "checkins",
            vec!["id", "session_id", "person_id", "present"],
            checkins,
        ),
    ])
}

/// The shape under test, with `{on}` the entries join key: `m.person_id`
/// folds the branch, `m.person_id + 0` is not a grouping column and keeps
/// the query as written.
fn fanout(on: &str) -> String {
    format!(
        "SELECT m.grp, m.person_id, p.score, \
         LEAST(50, COALESCE(AVG(e.progress), 0)), MIN(e.progress), MAX(e.id), \
         COUNT(DISTINCT r.target_id), COUNT(DISTINCT s.id), COUNT(DISTINCT se.id), \
         SUM(CASE WHEN a.id IS NOT NULL AND a.present = 1 THEN 1 ELSE 0 END), \
         COUNT(*), COUNT(a.id), AVG(se.at), MAX(se.at) \
         FROM members m \
         JOIN persons p ON p.id = m.person_id \
         LEFT JOIN entries e ON e.person_id = {on} AND e.progress > 3 \
         LEFT JOIN grps g ON g.id = m.grp \
         LEFT JOIN items i ON i.plan_id = g.plan_id \
         LEFT JOIN clips c ON c.item_id = i.id \
         LEFT JOIN refs r ON r.target_id = c.ref_id AND r.live = 1 \
         LEFT JOIN receipts s ON s.target_id = r.target_id AND s.person_id = m.person_id \
         LEFT JOIN sessions se ON se.item_id = i.id AND se.at >= COALESCE(m.since, 0) \
         LEFT JOIN checkins a ON a.session_id = se.id AND a.person_id = m.person_id \
         WHERE m.grp IN (1, 2, 3, 5, 8) AND m.active = 1 \
         GROUP BY m.grp, m.person_id, p.score \
         HAVING COUNT(*) > 1 AND COALESCE(SUM(a.present), 0) >= 0"
    )
}

#[test]
fn folded_branch_matches_the_join_as_written() {
    let fixture = fixture(40, 40, 6, 4);
    let (folded, _, profile) = fixture.run(&fanout("m.person_id"));
    let (written, _, _) = fixture.run(&fanout("m.person_id + 0"));
    assert_ne!(folded, [] as [std::string::String; 0]);
    assert_eq!(folded, written);
    assert!(
        profile.matches("HashAggregate").count() >= 2,
        "the entries branch folds before its join:\n{profile}"
    );
}

#[test]
#[ignore = "measurement: run with --ignored --nocapture"]
fn fanout_join_aggregates_measure() {
    let fixture = fixture(80, 20, 60, 20);
    for on in ["m.person_id + 0", "m.person_id"] {
        let (rows, elapsed, profile) = fixture.run(&fanout(on));
        println!(
            "ON {on}: {} groups in {elapsed:.1} ms\n{profile}",
            rows.len()
        );
    }
}
