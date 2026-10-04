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
    answer(
        ROWS,
        |id| {
            let (state, kind, mark, n) = row(id);
            (
                state.to_owned(),
                kind.map(str::to_owned),
                mark.to_owned(),
                n,
            )
        },
        sql,
    )
    .0
}

/// The statement's rows, sorted, over a table of `rows` rows built by
/// `make`, and what its operators noted of the path they took.
fn answer(
    rows: u64,
    make: impl Fn(u64) -> (String, Option<String>, String, i64),
    sql: &str,
) -> (Vec<Vec<String>>, String) {
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
    table
        .bulk_ingest_snapshot(
            (1..=rows)
                .map(|id| {
                    let (state, kind, mark, n) = make(id);
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            Value::Utf8(state),
                            kind.map_or(Value::Null, Value::Utf8),
                            Value::Utf8(mark),
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
        TableStatistics::with_row_count(rows),
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
        Execution::start_profiled(physical, &provider, 1 << 30, None, Collation::default())
            .expect("start");
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
    let notes = execution
        .profile()
        .expect("profile")
        .operators
        .iter()
        .filter_map(|operator| operator.note.clone())
        .collect::<Vec<_>>()
        .join("; ");
    (rows, notes)
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

/// Two text keys of a few classes fold by class - a table indexed by the
/// two class ids - rather than by cutting and hashing every row: a state
/// by a kind is a dozen groups however many rows hold them.
#[test]
fn two_text_keys_of_few_classes_fold_by_class() {
    let make = |id| {
        let (state, kind, mark, n) = row(id);
        (
            state.to_owned(),
            kind.map(str::to_owned),
            mark.to_owned(),
            n,
        )
    };
    for sql in [
        "SELECT state, kind, COUNT(*), SUM(n) FROM tickets GROUP BY state, kind",
        "SELECT DISTINCT state, mark FROM tickets",
    ] {
        let (_, notes) = answer(ROWS, make, sql);
        assert!(
            notes.contains(&format!("{ROWS} rows folded by text class")),
            "{sql}: {notes}"
        );
    }
}

/// Keys that start as a few classes and grow past what the table by class
/// holds: the groups folded by class move to the hashed fold and go on
/// there, each still showing its own first row's spellings and counting
/// every row once.
#[test]
fn text_keys_that_outgrow_the_table_by_class_keep_their_groups() {
    const MANY: u64 = 300_000;
    let make = |id: u64| {
        if id <= 200_000 {
            let (state, kind, mark, n) = row(id);
            (
                state.to_owned(),
                kind.map(str::to_owned),
                mark.to_owned(),
                n,
            )
        } else {
            // Ninety states by seventy kinds, each in two spellings, and the
            // early rows' classes among them.
            let state = match id % 90 {
                0 => "OPEN".to_owned(),
                1 => "closed".to_owned(),
                other if id.is_multiple_of(2) => format!("s{other}"),
                other => format!("S{other}"),
            };
            let kind = match id % 70 {
                0 => "B".to_owned(),
                other if id.is_multiple_of(3) => format!("k{other}"),
                other => format!("K{other}"),
            };
            (state, Some(kind), "x".to_owned(), row(id).3)
        }
    };
    let fold = |text: &Option<String>| text.as_deref().map(str::to_lowercase);
    let mut groups = BTreeMap::new();
    for id in 1..=MANY {
        let (state, kind, _, n) = make(id);
        let group = groups
            .entry((Some(state.to_lowercase()), fold(&kind)))
            .or_insert((
                state,
                kind.unwrap_or_else(|| "NULL".to_owned()),
                0_u64,
                0_i64,
            ));
        group.2 += 1;
        group.3 += n;
    }
    let (rows, notes) = answer(
        MANY,
        make,
        "SELECT state, kind, COUNT(*), SUM(n) FROM tickets GROUP BY state, kind",
    );
    assert_eq!(
        rows,
        sorted(
            groups
                .into_values()
                .map(|(state, kind, count, sum)| vec![
                    state,
                    kind,
                    count.to_string(),
                    sum.to_string()
                ])
                .collect()
        ),
        "{notes}"
    );
    // Some rows by class, the rest hashed.
    assert!(notes.contains("rows folded by text class"), "{notes}");
    assert!(!notes.contains(" 0 rows folded by text class"), "{notes}");
    assert!(
        !notes.contains(&format!("{MANY} rows folded by text class")),
        "{notes}"
    );
}

/// Spellings of the classes a later table's groups are made of.
const CLASSES: [[&str; 3]; 6] = [
    ["alpha", "Alpha", "ALPHA"],
    ["beta", "Beta", "BETA"],
    ["gamma", "Gamma", "GAMMA"],
    ["delta", "Delta", "DELTA"],
    ["omega", "Omega", "OMEGA"],
    ["sigma", "Sigma", "SIGMA"],
];
const SEGMENT_ROWS: u64 = 512;
const SEGMENTS: u64 = 96;

/// `(state, kind)` of one row of a table whose first segment spells every
/// class it will hold, each beside a key no later row has, and whose later
/// segments then meet the groups in no order a worker would choose: each
/// segment holds a few of them, in a spelling of its own.
fn scattered(id: u64) -> (String, String) {
    let at = usize::try_from(id).expect("small");
    if id < SEGMENT_ROWS {
        let spelling = CLASSES[at % 6][(at / 6) % 3];
        return if at % 2 == 0 {
            (spelling.to_owned(), "seed".to_owned())
        } else {
            ("seed".to_owned(), spelling.to_owned())
        };
    }
    let segment = id / SEGMENT_ROWS;
    let group = (segment.wrapping_mul(0x9E37_79B9) >> 7) as usize % 36 + at % 3;
    let variant = usize::try_from(segment).expect("small") % 3;
    (
        CLASSES[group % 6][variant].to_owned(),
        CLASSES[(group / 6) % 6][(variant + 1) % 3].to_owned(),
    )
}

/// The workers of a round fold its slices in no order, each into a copy of
/// the table of its own kept across rounds: a worker that folds a later
/// slice first must still give a group the spellings of an earlier one.
#[test]
#[allow(clippy::too_many_lines)] // the table, the expected groups, then the runs
fn a_group_first_met_mid_round_shows_its_first_rows_spellings() {
    let directory = tempfile::tempdir().expect("directory");
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("table");
    for segment in 0..SEGMENTS {
        let rows = (segment * SEGMENT_ROWS..(segment + 1) * SEGMENT_ROWS)
            .map(|id| {
                let (state, kind) = scattered(id);
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                    vec![
                        Value::UInt64(id),
                        Value::Utf8(state),
                        Value::Utf8(kind),
                        Value::Utf8("x".to_owned()),
                        Value::Int64(1),
                    ],
                    id + 1,
                    false,
                )
            })
            .collect();
        table.ingest(rows).expect("rows");
        table.flush().expect("flush");
    }
    let mut groups = BTreeMap::new();
    for id in 0..SEGMENTS * SEGMENT_ROWS {
        let (state, kind) = scattered(id);
        groups
            .entry((state.to_lowercase(), kind.to_lowercase()))
            .or_insert((state, kind, 0_u64))
            .2 += 1;
    }
    let expected = sorted(
        groups
            .into_values()
            .map(|(state, kind, count)| vec![state, kind, count.to_string()])
            .collect(),
    );
    let entry = TableEntry::new(
        TableId::new(1),
        "tickets",
        schema(),
        TableStatistics::with_row_count(SEGMENTS * SEGMENT_ROWS),
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
    // One run: a repeat is answered from the settled result, not folded
    // again. Which worker reaches which slice changes from run to run, so
    // this guards the path; the unit tests beside the fold pin each order.
    {
        let sql = "SELECT state, kind, COUNT(*) FROM tickets GROUP BY state, kind";
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start_profiled(physical, &provider, 1 << 30, None, Collation::default())
                .expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                rows.push(
                    (0..batch.columns().len())
                        .map(|column| {
                            render(
                                batch
                                    .column(column)
                                    .and_then(|column| column.value(row))
                                    .expect("value"),
                            )
                        })
                        .collect(),
                );
            }
        }
        let notes = execution
            .profile()
            .expect("profile")
            .operators
            .iter()
            .filter_map(|operator| operator.note.clone())
            .collect::<Vec<_>>()
            .join("; ");
        assert!(
            notes.contains("folded in place") && notes.contains("rows folded by text class"),
            "{notes}"
        );
        assert_eq!(sorted(rows), expected, "{notes}");
    }
}
