//! Two text keys show each group's own first row, not each key's class.
//!
//! Under a case-insensitive collation `'Open'`, `'OPEN'` and `'open'` are
//! one value, and a group shows the spelling of its first row. With one key
//! the group is the class, so the first spelling met of the class is the
//! group's. With two keys a group is a pair of classes: `('OPEN', 'b')`
//! after `('Open', 'A')` is a new group, and it reads `OPEN` - the spelling
//! its own first row carries - not the `Open` the class was first met as.
//! The fold for two text keys answered every group with each class's first
//! spelling, for `GROUP BY` and for `DISTINCT`.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const AI_CI: &str = "utf8mb4_0900_ai_ci";
const ROWS: u64 = 6_000;
const STATES: [&str; 5] = ["open", "Open", "OPEN", "closed", "Closed"];
const KINDS: [&str; 3] = ["a", "A", "b"];
const MARKS: [&str; 4] = ["x", "X", "y", "Y"];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "state", DataType::Utf8, false).with_collation(Some(AI_CI.to_owned())),
            Column::new(3, "kind", DataType::Utf8, true).with_collation(Some(AI_CI.to_owned())),
            Column::new(4, "mark", DataType::Utf8, false).with_collation(Some(AI_CI.to_owned())),
            Column::new(5, "n", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn pick(values: &[&'static str], id: u64) -> &'static str {
    values[usize::try_from(id).expect("small") % values.len()]
}

/// `(state, kind, mark, n)` of one row.
fn row(id: u64) -> (&'static str, Option<&'static str>, &'static str, i64) {
    (
        pick(&STATES, id),
        (!id.is_multiple_of(11)).then(|| pick(&KINDS, id)),
        pick(&MARKS, id),
        i64::try_from(id % 7).expect("small"),
    )
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Utf8(text) | Value::Enum { label: text, .. } => text.clone(),
        Value::UInt64(number) => number.to_string(),
        Value::Int64(number) => number.to_string(),
        other => format!("{other:?}"),
    }
}

fn run(sql: &str) -> Vec<Vec<String>> {
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
    table
        .bulk_ingest_snapshot(
            (1..=ROWS)
                .map(|id| {
                    let (state, kind, mark, n) = row(id);
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            Value::Utf8(state.to_owned()),
                            kind.map_or(Value::Null, |kind| Value::Utf8(kind.to_owned())),
                            Value::Utf8(mark.to_owned()),
                            Value::Int64(n),
                        ],
                        id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("rows");
    let entry = TableEntry::new(
        TableId::new(1),
        "tickets",
        schema(),
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
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
        .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            rows.push(
                (0..batch.columns().len())
                    .map(|column| {
                        render(
                            batch
                                .column(column)
                                .and_then(|column| column.value(row))
                                .expect("selected value"),
                        )
                    })
                    .collect(),
            );
        }
    }
    rows.sort();
    rows
}

/// The groups of `(first, second)` over the rows from `from` on, each with
/// the spellings of its first row, its row count, the sum of `n` and the
/// smallest id: what a scan in key order that keeps a group's first row
/// answers.
fn expected(
    from: u64,
    keys: impl Fn(u64) -> (Option<&'static str>, Option<&'static str>),
) -> Vec<(String, String, u64, i64, u64)> {
    let fold = |text: Option<&str>| text.map(str::to_lowercase);
    let mut groups = BTreeMap::new();
    for id in from..=ROWS {
        let (first, second) = keys(id);
        let group = groups.entry((fold(first), fold(second))).or_insert((
            first.unwrap_or("NULL").to_owned(),
            second.unwrap_or("NULL").to_owned(),
            0_u64,
            0_i64,
            id,
        ));
        group.2 += 1;
        group.3 += row(id).3;
    }
    groups.into_values().collect()
}

fn sorted(mut rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    rows.sort();
    rows
}

#[test]
fn a_group_of_two_text_keys_shows_its_first_rows_spellings() {
    let state_kind = |id| (Some(row(id).0), row(id).1);
    // From the first row, and from a later one: the filter changes which
    // row is first in a group, and so which spelling the group shows.
    for from in [1, 8] {
        let groups = expected(from, state_kind);
        assert_eq!(
            run(&format!(
                "SELECT state, kind, COUNT(*) FROM tickets WHERE id >= {from} \
                 GROUP BY state, kind"
            )),
            sorted(
                groups
                    .iter()
                    .map(|(state, kind, count, ..)| vec![
                        state.clone(),
                        kind.clone(),
                        count.to_string()
                    ])
                    .collect()
            ),
            "counts from {from}"
        );
        assert_eq!(
            run(&format!(
                "SELECT state, kind, SUM(n), MIN(id) FROM tickets WHERE id >= {from} \
                 GROUP BY state, kind"
            )),
            sorted(
                groups
                    .iter()
                    .map(|(state, kind, _, sum, first)| vec![
                        state.clone(),
                        kind.clone(),
                        sum.to_string(),
                        first.to_string()
                    ])
                    .collect()
            ),
            "totals from {from}"
        );
    }
    // The keys the other way round are the same groups.
    assert_eq!(
        run("SELECT kind, state, COUNT(*) FROM tickets GROUP BY kind, state"),
        sorted(
            expected(1, state_kind)
                .into_iter()
                .map(|(state, kind, count, ..)| vec![kind, state, count.to_string()])
                .collect()
        )
    );
}

#[test]
fn distinct_over_two_text_columns_shows_each_rows_own_spellings() {
    for from in [1, 4] {
        assert_eq!(
            run(&format!(
                "SELECT DISTINCT state, mark FROM tickets WHERE id >= {from}"
            )),
            sorted(
                expected(from, |id| (Some(row(id).0), Some(row(id).2)))
                    .into_iter()
                    .map(|(state, mark, ..)| vec![state, mark])
                    .collect()
            ),
            "from {from}"
        );
    }
    assert_eq!(
        run("SELECT DISTINCT state, kind FROM tickets"),
        sorted(
            expected(1, |id| (Some(row(id).0), row(id).1))
                .into_iter()
                .map(|(state, kind, ..)| vec![state, kind])
                .collect()
        )
    );
}
