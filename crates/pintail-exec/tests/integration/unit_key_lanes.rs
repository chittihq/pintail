//! Grouped aggregates whose key or argument is read as packed units: a
//! `GROUP BY DATE(seen_at)` or a DATE column over hundreds of days, and a
//! COUNT of a text column or a MIN/MAX of a DATETIME under many groups.
//!
//! The rows live in a settled segment, in the memtable only, or in a
//! segment with writes on top, and one layout adds rows holding zero dates:
//! a batch with one packs the zero date as units below every real date, and
//! it has to answer as the zero date rather than read as NULL or as a real
//! day. Expectations are computed from the generator;
//! the zero-date layout is also compared with the same query kept on the
//! general path by an aggregate no lane takes. ENUM and SET keys are
//! checked for their declared order under the same aggregates.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Declared order differs from alphabetical order.
const STATES: [&str; 4] = ["queued", "active", "paused", "closed"];
const MARKS: [&str; 3] = ["urgent", "bulky", "fragile"];
const MARK_SETS: [&str; 5] = ["", "fragile", "urgent", "urgent,fragile", "bulky"];
const TAGS: [&str; 6] = ["amber", "birch", "cedar", "delta", "ember", "flint"];
const ZERO_DATETIME: &str = "0000-00-00 00:00:00";
const ZERO_DATE: &str = "0000-00-00";

#[derive(Clone, Debug)]
struct Item {
    seen_at: Option<String>,
    due: Option<String>,
    tag: Option<&'static str>,
    state: &'static str,
    marks: &'static str,
    qty: Option<i64>,
    grp: i64,
}

fn mix(id: u64) -> u64 {
    let mut x = id.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Days since 1970-01-01 as a date.
fn date(days: u64) -> String {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn pick<const N: usize>(values: [&'static str; N], hash: u64) -> &'static str {
    values[usize::try_from(hash % N as u64).expect("small")]
}

fn item(id: u64, salt: u64) -> Item {
    let h = mix(id ^ (salt << 48));
    let seconds = (h >> 9) % 86_400;
    Item {
        seen_at: (!(id + salt).is_multiple_of(41)).then(|| {
            format!(
                "{} {:02}:{:02}:{:02}",
                date(19_700 + (h >> 3) % 400),
                seconds / 3_600,
                seconds / 60 % 60,
                seconds % 60
            )
        }),
        due: (!(id + salt).is_multiple_of(37)).then(|| date(19_000 + (h >> 20) % 900)),
        tag: (!(id + salt).is_multiple_of(5)).then(|| pick(TAGS, h >> 30)),
        state: pick(STATES, h >> 34),
        marks: pick(MARK_SETS, h >> 38),
        qty: (!(id + salt).is_multiple_of(11))
            .then(|| i64::try_from((h >> 42) % 2_001).expect("small") - 1_000),
        grp: i64::try_from(id % 3_000).expect("small"),
    }
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "seen_at", DataType::DateTime64 { fsp: 0 }, true),
            Column::new(3, "due", DataType::Date32, true),
            Column::new(4, "tag", DataType::Utf8, true),
            Column::new(5, "state", DataType::Utf8, false)
                .with_enum_labels(Some(STATES.iter().map(ToString::to_string).collect())),
            Column::new(6, "marks", DataType::Utf8, false)
                .with_set_members(Some(MARKS.iter().map(ToString::to_string).collect())),
            Column::new(7, "qty", DataType::Int64, true),
            Column::new(8, "grp", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn stored(id: u64, item: &Item, version: u64) -> StoredRow {
    let text = |value: &Option<String>| value.clone().map_or(Value::Null, Value::Utf8);
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            text(&item.seen_at),
            text(&item.due),
            item.tag
                .map_or(Value::Null, |tag| Value::Utf8(tag.to_owned())),
            Value::Utf8(item.state.to_owned()),
            Value::Utf8(item.marks.to_owned()),
            item.qty.map_or(Value::Null, Value::Int64),
            Value::Int64(item.grp),
        ],
        version,
        false,
    )
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    Settled,
    MemtableOnly,
    SettledWithWrites,
    /// A settled segment, then memtable rows of which some hold zero dates.
    SettledWithZeroDates,
    /// A settled segment of plain dates, then a second, flushed segment of
    /// later keys in which some rows hold zero dates: the scan's first
    /// batches carry units and a later one does not.
    ZeroDatesInALaterSegment,
}

type Model = BTreeMap<u64, Item>;

fn build(layout: Layout, rows: u64) -> (tempfile::TempDir, TableStore, Model) {
    let directory = tempfile::tempdir().expect("directory");
    let mut table = TableStore::open(
        directory.path(),
        schema(),
        StoreOptions {
            background_compaction: false,
            ..StoreOptions::default()
        },
    )
    .expect("table");
    let mut model = Model::new();
    let base = (0..rows)
        .map(|id| {
            let item = item(id, 0);
            let row = stored(id, &item, 1);
            model.insert(id, item);
            row
        })
        .collect::<Vec<_>>();
    match layout {
        Layout::MemtableOnly => {
            table.ingest(base).expect("memtable rows");
        }
        _ => {
            table.bulk_ingest_snapshot(base).expect("ingest");
        }
    }
    if matches!(
        layout,
        Layout::SettledWithWrites | Layout::SettledWithZeroDates
    ) {
        let mut writes = Vec::new();
        for id in (0..rows + 40).step_by(7) {
            let item = item(id, 1);
            writes.push(stored(id, &item, 2));
            model.insert(id, item);
        }
        if matches!(layout, Layout::SettledWithZeroDates) {
            for id in (3..rows).step_by(usize::try_from(rows / 13).expect("step").max(1)) {
                let mut item = item(id, 2);
                item.seen_at = Some(ZERO_DATETIME.to_owned());
                if id % 2 == 1 {
                    item.due = Some(ZERO_DATE.to_owned());
                }
                writes.push(stored(id, &item, 3));
                model.insert(id, item);
            }
        }
        table.ingest(writes).expect("memtable writes");
    }
    if matches!(layout, Layout::ZeroDatesInALaterSegment) {
        let mut later = Vec::new();
        for id in rows..rows + 3_000 {
            let mut item = item(id, 3);
            if id % 5 == 0 {
                item.seen_at = Some(ZERO_DATETIME.to_owned());
            }
            if id % 7 == 0 {
                item.due = Some(ZERO_DATE.to_owned());
            }
            later.push(stored(id, &item, 2));
            model.insert(id, item);
        }
        table.ingest(later).expect("later rows");
        table.flush().expect("flush");
    }
    (directory, table, model)
}

fn run(table: &TableStore, rows: u64, sql: &str) -> Vec<Vec<String>> {
    run_noted(table, rows, sql).0
}

/// The answer, and what the operators noted about how they ran.
fn run_noted(table: &TableStore, rows: u64, sql: &str) -> (Vec<Vec<String>>, String) {
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(1);
    let table_id = TableId::new(1);
    let entry = TableEntry::new(
        table_id,
        "parcels",
        schema(),
        TableStatistics::with_row_count(rows),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start_profiled(physical, &provider, 512 << 20, None, Collation::default())
            .expect("start");
    let mut out = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            out.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value_owned(row).expect("value") {
                        Value::Null => "NULL".to_owned(),
                        Value::Utf8(text) | Value::Enum { label: text, .. } => text,
                        Value::UInt64(number) => number.to_string(),
                        Value::Int64(number) => number.to_string(),
                        Value::DecimalAverage(average) => average.label.clone(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>(),
            );
        }
    }
    let notes = execution
        .profile()
        .map(|profile| {
            profile
                .operators
                .iter()
                .filter_map(|operator| operator.note.clone())
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();
    (out, notes)
}

fn text(value: Option<impl ToString>) -> String {
    value.map_or_else(|| "NULL".to_owned(), |value| value.to_string())
}

/// AVG of integers: four places, rounded half away from zero.
fn average(sum: i64, count: i64) -> String {
    let scaled = i128::from(sum) * 10_000;
    let count = i128::from(count);
    let units = (scaled.abs() * 2 + count) / (count * 2);
    format!(
        "{}{}.{:04}",
        if scaled < 0 && units != 0 { "-" } else { "" },
        units / 10_000,
        units % 10_000
    )
}

/// `COUNT(*), COUNT(tag), MIN(seen_at), MAX(seen_at), MIN(due), MAX(due),
/// COUNT(qty), SUM(qty), AVG(qty), MIN(qty), MAX(qty)` over `items`.
fn totals(items: &[&Item]) -> Vec<String> {
    let seen = || items.iter().filter_map(|item| item.seen_at.as_deref());
    let due = || items.iter().filter_map(|item| item.due.as_deref());
    let qty = || items.iter().filter_map(|item| item.qty);
    let count = i64::try_from(qty().count()).expect("small");
    vec![
        items.len().to_string(),
        items
            .iter()
            .filter(|item| item.tag.is_some())
            .count()
            .to_string(),
        text(seen().min()),
        text(seen().max()),
        text(due().min()),
        text(due().max()),
        count.to_string(),
        text((count > 0).then(|| qty().sum::<i64>())),
        text((count > 0).then(|| average(qty().sum::<i64>(), count))),
        text(qty().min()),
        text(qty().max()),
    ]
}

const TOTALS: &str = "COUNT(*), COUNT(tag), MIN(seen_at), MAX(seen_at), MIN(due), MAX(due), \
                      COUNT(qty), SUM(qty), AVG(qty), MIN(qty), MAX(qty)";

/// [`TOTALS`] in two queries: the streaming two-pass takes at most seven
/// aggregates, so the whole list in one query stays on the general path.
const HALVES: [&str; 2] = [
    "COUNT(*), COUNT(tag), MIN(seen_at), MAX(seen_at), MIN(due), MAX(due)",
    "COUNT(qty), SUM(qty), AVG(qty), MIN(qty), MAX(qty)",
];

/// [`TOTALS`] by `key` through the lanes, sorted: the two halves joined on
/// the key, and the notes of both.
fn laned(table: &TableStore, rows: u64, key: &str) -> (Vec<Vec<String>>, String) {
    let half = |aggregates: &str| {
        let sql = format!("SELECT {key} AS k, {aggregates} FROM parcels GROUP BY k");
        let (rows, notes) = run_noted(table, rows, &sql);
        (sorted(rows), notes)
    };
    let (mut left, mut notes) = half(HALVES[0]);
    let (right, more) = half(HALVES[1]);
    assert_eq!(left.len(), right.len(), "{key}: the halves' groups differ");
    for (row, other) in left.iter_mut().zip(right) {
        assert_eq!(row[0], other[0], "{key}: the halves' keys differ");
        row.extend(other.into_iter().skip(1));
    }
    notes.push_str("; ");
    notes.push_str(&more);
    (left, notes)
}

fn expected(model: &Model, key: impl Fn(&Item) -> String) -> Vec<Vec<String>> {
    let mut groups = BTreeMap::<String, Vec<&Item>>::new();
    for item in model.values() {
        groups.entry(key(item)).or_default().push(item);
    }
    groups
        .into_iter()
        .map(|(key, items)| {
            let mut row = vec![key];
            row.extend(totals(&items));
            row
        })
        .collect()
}

fn sorted(mut rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    rows.sort();
    rows
}

fn each(mut check: impl FnMut(&str, &TableStore, u64, &Model, bool)) {
    for rows in [900_u64, 150_000] {
        for layout in [
            Layout::Settled,
            Layout::MemtableOnly,
            Layout::SettledWithWrites,
            Layout::SettledWithZeroDates,
            Layout::ZeroDatesInALaterSegment,
        ] {
            let (_directory, table, model) = build(layout, rows);
            check(
                &format!("{layout:?} at {rows} rows"),
                &table,
                rows + 3_000,
                &model,
                matches!(
                    layout,
                    Layout::SettledWithZeroDates | Layout::ZeroDatesInALaterSegment
                ),
            );
        }
    }
}

/// The same query kept on the general path: one more aggregate, which no
/// lane takes, dropped from the answer.
fn general(table: &TableStore, rows: u64, key: &str) -> Vec<Vec<String>> {
    let sql = format!(
        "SELECT {key} AS k, {TOTALS}, GROUP_CONCAT(DISTINCT state) FROM parcels GROUP BY k"
    );
    run(table, rows, &sql)
        .into_iter()
        .map(|mut row| {
            row.pop();
            row
        })
        .collect()
}

#[test]
fn a_day_key_and_a_date_column_key_match_the_generator() {
    each(|label, table, rows, model, zero_dates| {
        for (key, of) in [
            (
                "DATE(seen_at)",
                (|item: &Item| text(item.seen_at.as_deref().map(|seen| &seen[..10])))
                    as fn(&Item) -> String,
            ),
            ("due", |item: &Item| text(item.due.as_deref())),
        ] {
            let (got, _) = laned(table, rows, key);
            assert_eq!(
                got,
                sorted(general(table, rows, key)),
                "{label}: {key} against the general path"
            );
            if !zero_dates || key == "due" {
                // A zero datetime's day is whatever the general path says
                // it is, compared above; everything else is the model's.
                assert_eq!(got, expected(model, of), "{label}: {key}");
            }
        }
    });
}

/// Whether no batch of a query folded row by row, as its notes report.
fn rode_the_lanes(notes: &str) -> bool {
    !notes
        .replace(" 0 batches folded row by row", "")
        .contains("folded row by row")
}

/// The lanes must be what answered, the batch holding zero dates included:
/// the zero date packs as units below every real date, so a batch holding
/// one arriving in the middle of a lane's stream rides the lanes too, and
/// nothing folds row by row.
#[test]
fn a_later_batch_with_zero_dates_rides_the_lanes() {
    let rows = 150_000;
    let (_directory, table, model) = build(Layout::ZeroDatesInALaterSegment, rows);
    let total = rows + 3_000;
    let by_group = expected(&model, |item| item.grp.to_string());
    for (sql, columns, of) in [
        (
            "SELECT grp, COUNT(tag), MIN(seen_at), MAX(seen_at), MIN(due) FROM parcels \
             GROUP BY grp",
            vec![0, 2, 3, 4, 5],
            &by_group,
        ),
        (
            "SELECT due, COUNT(*), COUNT(tag), MIN(seen_at) FROM parcels GROUP BY due",
            vec![0, 1, 2, 3],
            &expected(&model, |item| text(item.due.as_deref())),
        ),
        (
            "SELECT state, COUNT(tag), MAX(seen_at) FROM parcels GROUP BY state",
            vec![0, 2, 4],
            &expected(&model, |item| item.state.to_owned()),
        ),
    ] {
        let (got, notes) = run_noted(&table, total, sql);
        let want = of
            .iter()
            .map(|row| columns.iter().map(|column| row[*column].clone()).collect())
            .collect::<Vec<Vec<String>>>();
        assert_eq!(sorted(got), sorted(want), "{sql}");
        assert!(
            rode_the_lanes(&notes),
            "{sql}: the batch holding zero dates left the lanes: {notes}"
        );
    }
    // A zero datetime's day, as the general path names it.
    let (got, notes) = laned(&table, total, "DATE(seen_at)");
    assert_eq!(got, sorted(general(&table, total, "DATE(seen_at)")));
    assert!(
        notes.contains("the key's packed units") && rode_the_lanes(&notes),
        "DATE(seen_at): {notes}"
    );
    // With no zero date anywhere the same key is the lanes' alone.
    let (_plain_directory, plain, plain_model) = build(Layout::Settled, rows);
    let (got, notes) = laned(&plain, rows, "DATE(seen_at)");
    assert_eq!(
        got,
        expected(&plain_model, |item| text(
            item.seen_at.as_deref().map(|seen| &seen[..10])
        ))
    );
    assert!(
        notes.contains("the key's packed units") && !notes.contains("row by row"),
        "DATE(seen_at), settled: {notes}"
    );
    // Date-part keys and a pair of text keys, against the general path: a
    // zero date's year and month are whatever the expressions say.
    for keys in ["YEAR(seen_at), MONTH(seen_at)", "state, tag"] {
        let lanes = "COUNT(*), COUNT(tag), MIN(seen_at), MAX(due)";
        let sql = format!("SELECT {keys}, {lanes} FROM parcels GROUP BY {keys}");
        let (got, notes) = run_noted(&table, total, &sql);
        let general = run(
            &table,
            total,
            &format!(
                "SELECT {keys}, {lanes}, GROUP_CONCAT(DISTINCT state) FROM parcels \
                 GROUP BY {keys}"
            ),
        )
        .into_iter()
        .map(|mut row| {
            row.pop();
            row
        })
        .collect();
        assert_eq!(sorted(got), sorted(general), "{sql}");
        assert!(rode_the_lanes(&notes), "{sql}: {notes}");
    }
}

#[test]
fn many_integer_groups_count_text_and_keep_temporal_extremes() {
    each(|label, table, rows, model, _| {
        let want = expected(model, |item| item.grp.to_string());
        assert_eq!(laned(table, rows, "grp").0, sorted(want), "{label}");
        // The lanes alone, so the packed folds take the whole query.
        let sql = "SELECT grp, COUNT(tag), MIN(seen_at), MAX(due) FROM parcels GROUP BY grp";
        let want = expected(model, |item| item.grp.to_string())
            .into_iter()
            .map(|row| {
                vec![
                    row[0].clone(),
                    row[2].clone(),
                    row[3].clone(),
                    row[6].clone(),
                ]
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sorted(run(table, rows, sql)),
            sorted(want),
            "{label}: lanes"
        );
    });
}

#[test]
fn enum_and_set_keys_keep_their_declared_order_under_the_new_lanes() {
    each(|label, table, rows, model, _| {
        let got = run(
            table,
            rows,
            "SELECT state, COUNT(tag), MIN(seen_at), MAX(seen_at) FROM parcels \
             GROUP BY state ORDER BY state",
        );
        let by_state = expected(model, |item| item.state.to_owned());
        let want = STATES
            .iter()
            .filter_map(|state| by_state.iter().find(|row| row[0] == *state))
            .map(|row| {
                vec![
                    row[0].clone(),
                    row[2].clone(),
                    row[3].clone(),
                    row[4].clone(),
                ]
            })
            .collect::<Vec<_>>();
        assert_eq!(got, want, "{label}: ENUM key");

        let mask = |marks: &str| -> u64 {
            marks
                .split(',')
                .filter(|mark| !mark.is_empty())
                .map(|mark| {
                    1_u64
                        << MARKS
                            .iter()
                            .position(|declared| *declared == mark)
                            .expect("declared")
                })
                .sum()
        };
        let got = run(
            table,
            rows,
            "SELECT marks, COUNT(tag), MAX(due) FROM parcels GROUP BY marks ORDER BY marks",
        );
        let mut want = expected(model, |item| item.marks.to_owned())
            .into_iter()
            .map(|row| vec![row[0].clone(), row[2].clone(), row[6].clone()])
            .collect::<Vec<_>>();
        want.sort_by_key(|row| mask(&row[0]));
        assert_eq!(got, want, "{label}: SET key");
    });
}
