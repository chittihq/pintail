//! A correlated `IN (subquery)` whose members differ for every outer row -
//! the subquery compares its table's key with the outer row's, so each row
//! has a member set of its own - is answered by yes-or-no questions of the
//! subquery's table instead of by reading the members.
//!
//! Every answer is checked against the members themselves, computed here
//! row by row with `MySQL`'s three-valued rules: over no members `IN` is
//! FALSE whatever is tested; a NULL tested value against any member is
//! NULL; a member equal to the value is TRUE; otherwise a NULL member makes
//! it NULL, and only then is it FALSE. `NOT IN` negates, leaving NULL. The
//! data holds every case: a depot with one parcel (no other member), a
//! depot whose only equal member is the outer row itself, NULL labels and
//! weights among the members and among the tested values, NULL depots that
//! equal nothing, duplicates, and labels that differ only by case.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    ExecCounters, Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    take_exec_counters,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

#[derive(Clone, Copy, Debug)]
struct Parcel {
    id: u64,
    depot: Option<i64>,
    label: Option<&'static str>,
    weight: Option<i64>,
}

const fn parcel(
    id: u64,
    depot: Option<i64>,
    label: Option<&'static str>,
    weight: Option<i64>,
) -> Parcel {
    Parcel {
        id,
        depot,
        label,
        weight,
    }
}

/// Depot 1: labels repeat, differ by case, and one is NULL. Depot 2: one
/// parcel. Depot 3: two parcels with different labels, so each one's only
/// equal member is itself. Depot 4: no NULL label. Depot 5: only NULL
/// labels. NULL depot: equal to nothing, itself included.
fn parcels() -> Vec<Parcel> {
    vec![
        parcel(1, Some(1), Some("red"), Some(5)),
        parcel(2, Some(1), Some("Red"), Some(7)),
        parcel(3, Some(1), Some("blue"), None),
        parcel(4, Some(1), None, Some(5)),
        parcel(5, Some(1), Some("green"), Some(9)),
        parcel(6, Some(2), Some("red"), Some(5)),
        parcel(7, Some(3), Some("amber"), Some(1)),
        parcel(8, Some(3), Some("black"), Some(1)),
        parcel(9, Some(4), Some("teal"), Some(3)),
        parcel(10, Some(4), Some("teal"), Some(4)),
        parcel(11, Some(4), Some("pink"), Some(4)),
        parcel(12, Some(5), None, None),
        parcel(13, Some(5), None, Some(2)),
        parcel(14, None, Some("red"), Some(5)),
        parcel(15, None, None, None),
        parcel(16, Some(1), Some("green"), Some(9)),
    ]
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "depot", DataType::Int64, true),
            Column::new(3, "label", DataType::Utf8, true),
            Column::new(4, "weight", DataType::Int64, true),
        ],
    )
    .expect("schema")
}

/// Runs `sql` over the parcels, and says what the execution counted.
fn run(sql: &str) -> (Vec<Vec<Value>>, ExecCounters) {
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
    let rows = parcels();
    table
        .bulk_ingest_snapshot(
            rows.iter()
                .map(|row| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(row.id)]).expect("key"),
                        vec![
                            Value::UInt64(row.id),
                            row.depot.map_or(Value::Null, Value::Int64),
                            row.label
                                .map_or(Value::Null, |label| Value::Utf8(label.to_owned())),
                            row.weight.map_or(Value::Null, Value::Int64),
                        ],
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TableId::new(1),
        "parcels",
        schema(),
        TableStatistics::with_row_count(rows.len() as u64),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let _ = take_exec_counters();
    let mut execution =
        Execution::start(plan, &provider, 256 << 20, Collation::default()).expect("execution");
    let mut answer = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            answer.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| column.value_owned(row).expect("value"))
                    .collect(),
            );
        }
    }
    (answer, take_exec_counters())
}

/// A truth value as the executor answers it: NULL is `None`.
fn truth(value: &Value) -> Option<bool> {
    match value {
        Value::Null => None,
        Value::Boolean(value) => Some(*value),
        Value::Int64(value) => Some(*value != 0),
        Value::UInt64(value) => Some(*value != 0),
        other => panic!("not a truth value: {other:?}"),
    }
}

/// What is tested and selected: the label, compared without case, or the
/// weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Member {
    Label,
    Weight,
}

impl Member {
    const fn column(self) -> &'static str {
        match self {
            Self::Label => "label",
            Self::Weight => "weight",
        }
    }

    fn of(self, row: &Parcel) -> Option<String> {
        match self {
            Self::Label => row.label.map(str::to_lowercase),
            Self::Weight => row.weight.map(|weight| weight.to_string()),
        }
    }
}

/// How the subquery's row is compared with the outer row beyond the depot,
/// as SQL and as the model: `None` when the comparison is NULL.
type Residual = (&'static str, fn(&Parcel, &Parcel) -> Option<bool>);

const RESIDUALS: [Residual; 5] = [
    ("q.id <> p.id", |q, p| Some(q.id != p.id)),
    ("q.id > p.id", |q, p| Some(q.id > p.id)),
    ("q.id < p.id", |q, p| Some(q.id < p.id)),
    ("q.weight <> p.weight", |q, p| Some(q.weight? != p.weight?)),
    ("q.id <> p.id AND q.weight >= 4", |q, p| {
        Some(q.id != p.id && q.weight? >= 4)
    }),
];

/// `IN` over the members themselves.
fn model(outer: &Parcel, member: Member, residual: Residual) -> Option<bool> {
    let members = parcels()
        .into_iter()
        .filter(|inner| {
            inner.depot.is_some()
                && inner.depot == outer.depot
                && residual.1(inner, outer) == Some(true)
        })
        .map(|inner| member.of(&inner))
        .collect::<Vec<_>>();
    if members.is_empty() {
        return Some(false);
    }
    let tested = member.of(outer)?;
    if members.iter().flatten().any(|value| *value == tested) {
        return Some(true);
    }
    if members.iter().any(Option::is_none) {
        return None;
    }
    Some(false)
}

#[test]
fn in_and_not_in_answer_as_their_members_do() {
    let mut failures = Vec::new();
    for member in [Member::Label, Member::Weight] {
        for residual in RESIDUALS {
            let column = member.column();
            let subquery = format!(
                "(SELECT q.{column} FROM parcels q WHERE q.depot = p.depot AND {})",
                residual.0
            );
            let sql = format!(
                "SELECT p.id, p.{column} IN {subquery}, p.{column} NOT IN {subquery} \
                 FROM parcels p ORDER BY p.id"
            );
            let (answer, counters) = run(&sql);
            let rows = parcels();
            assert_eq!(answer.len(), rows.len(), "{sql}");
            for (row, outer) in answer.iter().zip(&rows) {
                let expected = model(outer, member, residual);
                let got = (truth(&row[1]), truth(&row[2]));
                if got != (expected, expected.map(|value| !value)) {
                    failures.push(format!(
                        "{sql}: parcel {}: {got:?}, the members say {expected:?}",
                        outer.id
                    ));
                }
            }
            // Both subqueries of every row were answered by the probes.
            assert_eq!(counters.membership_probe_builds, 2, "{sql}");
            assert_eq!(counters.membership_probes, 2 * rows.len() as u64, "{sql}");
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The same tests deciding which rows a `WHERE` keeps: only TRUE keeps one.
#[test]
fn a_filter_keeps_the_rows_whose_test_is_true() {
    for member in [Member::Label, Member::Weight] {
        for residual in RESIDUALS {
            for negated in [false, true] {
                let column = member.column();
                let sql = format!(
                    "SELECT p.id FROM parcels p WHERE p.{column} {} \
                     (SELECT q.{column} FROM parcels q WHERE q.depot = p.depot AND {}) \
                     ORDER BY p.id",
                    if negated { "NOT IN" } else { "IN" },
                    residual.0
                );
                let expected = parcels()
                    .iter()
                    .filter(|outer| model(outer, member, residual) == Some(!negated))
                    .map(|outer| vec![Value::UInt64(outer.id)])
                    .collect::<Vec<_>>();
                assert_eq!(run(&sql).0, expected, "{sql}");
            }
        }
    }
}

/// Members that outer rows of one depot share are read once per depot, as
/// before: no probe is built for them.
#[test]
fn members_shared_by_a_key_are_not_probed() {
    let sql = "SELECT p.id, p.label NOT IN (SELECT q.label FROM parcels q \
               WHERE q.depot = p.depot) FROM parcels p ORDER BY p.id";
    let (answer, counters) = run(sql);
    assert_eq!(counters.membership_probe_builds, 0);
    assert_eq!(counters.membership_probes, 0);
    for (row, outer) in answer.iter().zip(&parcels()) {
        let expected = model(outer, Member::Label, ("", |_, _| Some(true)));
        assert_eq!(truth(&row[1]), expected.map(|value| !value), "{outer:?}");
    }
}
