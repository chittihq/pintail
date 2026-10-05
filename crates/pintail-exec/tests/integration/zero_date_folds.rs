//! A DATE or DATETIME column holding `MySQL`'s zero date keeps its packed
//! units: the zero date packs as the day before every real date, so the
//! groupings, distinct counts and extremes over it fold as they do over
//! real dates, and answer as `MySQL` does - the zero date its own group,
//! ordered before every real date.
//!
//! The answers are checked against a model computed here from the rows,
//! under `MySQL` 8.4's default mode: settled in segments (every one holding
//! zero dates), in the memtable, and settled with writes whose rows add a
//! date with a zero month. That one keeps its batch as text, so packed and
//! text batches meet in one grouping; the mode's copy check writes it as
//! the zero date, which must land in the zero date's group.

use std::collections::{BTreeMap, BTreeSet};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, ParseMode, parse_statement, with_parse_mode};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ZERO_DATE: &str = "0000-00-00";
const ZERO_DATETIME: &str = "0000-00-00 00:00:00";
const PARTIAL_DATE: &str = "2024-00-15";

/// One row as the model holds it.
#[derive(Clone, Debug)]
struct Event {
    day: Option<String>,
    at: Option<String>,
    amount: i64,
}

fn event(id: u64, partial: bool, days: u64) -> Event {
    let spread = (id.wrapping_mul(2_654_435_761) >> 7) % days;
    let first = pintail_types::parse_date_days("2020-01-01").expect("date");
    let real = pintail_types::format_date_days(first + i64::try_from(spread).expect("small"))
        .expect("real date");
    let clock = format!("{:02}:{:02}:{:02}", id % 24, id % 60, (id * 7) % 60);
    let (day, at) = if id.is_multiple_of(41) {
        (None, None)
    } else if id.is_multiple_of(97) {
        (Some(ZERO_DATE.to_owned()), Some(ZERO_DATETIME.to_owned()))
    } else {
        (Some(real.clone()), Some(format!("{real} {clock}")))
    };
    let day = if partial && id.is_multiple_of(5) {
        Some(PARTIAL_DATE.to_owned())
    } else {
        day
    };
    Event {
        day,
        at,
        amount: i64::try_from(id % 7).expect("small") - 3,
    }
}

fn stored(id: u64, event: &Event, version: u64) -> StoredRow {
    let text = |value: &Option<String>| value.clone().map_or(Value::Null, Value::Utf8);
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            text(&event.day),
            text(&event.at),
            Value::Int64(event.amount),
        ],
        version,
        false,
    )
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "day", DataType::Date32, true),
            Column::new(3, "at", DataType::DateTime64 { fsp: 0 }, true),
            Column::new(4, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    Settled,
    MemtableOnly,
    SettledWithPartialWrites,
}

struct Events {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
    model: BTreeMap<u64, Event>,
}

fn events(layout: Layout, rows: u64, days: u64) -> Events {
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
    let mut model = BTreeMap::new();
    let mut base = Vec::new();
    for id in 1..=rows {
        let event = event(id, false, days);
        base.push(stored(id, &event, 1));
        model.insert(id, event);
    }
    match layout {
        Layout::MemtableOnly => {
            table.ingest(base).expect("memtable rows");
        }
        Layout::Settled | Layout::SettledWithPartialWrites => {
            for chunk in base.chunks(20_000) {
                table.bulk_ingest_snapshot(chunk.to_vec()).expect("ingest");
            }
        }
    }
    if matches!(layout, Layout::SettledWithPartialWrites) {
        let mut writes = Vec::new();
        for id in (1..=rows).step_by(13) {
            let event = event(id, true, days);
            writes.push(stored(id, &event, 2));
            model.insert(id, event);
        }
        table.ingest(writes).expect("memtable writes");
    }
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TableId::new(1),
        "events",
        schema(),
        TableStatistics::with_row_count(rows),
    )
    .expect("entry")
    .with_column_statistics(pintail_catalog::LazyColumnStatistics::new(move || {
        snapshot.column_statistics()
    }));
    Events {
        _directory: directory,
        table,
        catalog: CatalogSnapshot::new([
            DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
        ])
        .expect("catalog"),
        model,
    }
}

/// `sql`'s rows rendered as text, sorted: these queries name no order.
fn run(events: &Events, sql: &str) -> Vec<Vec<String>> {
    pull(events, sql).0
}

/// `sql`'s sorted rows and its operators' profile notes.
fn pull(events: &Events, sql: &str) -> (Vec<Vec<String>>, String) {
    let mode = ParseMode::from_sql_mode(pintail_sql::DEFAULT_SQL_MODE);
    with_parse_mode(mode, || {
        let snapshot = events.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&events.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start_profiled(plan, &provider, 1 << 30, None, Collation::default())
                .expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| match column.value(row) {
                            Some(Value::Null) | None => "NULL".to_owned(),
                            Some(Value::UInt64(number)) => number.to_string(),
                            Some(Value::Int64(number)) => number.to_string(),
                            Some(other) => other
                                .text()
                                .map_or_else(|| format!("{other:?}"), ToOwned::to_owned),
                        })
                        .collect::<Vec<_>>(),
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
    })
}

fn text(value: Option<&str>) -> String {
    value.unwrap_or("NULL").to_owned()
}

/// A grouping or distinct key as the default mode copies it:
/// `NO_ZERO_IN_DATE` writes a date with a zero month as the zero date.
fn copied(day: Option<&String>) -> Option<String> {
    day.map(|day| {
        if day == PARTIAL_DATE {
            ZERO_DATE.to_owned()
        } else {
            day.clone()
        }
    })
}

fn sorted(mut rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    rows.sort();
    rows
}

#[allow(clippy::too_many_lines)]
fn check(events: &Events) {
    let model = &events.model;

    let mut by_day = BTreeMap::<Option<String>, (u64, i64)>::new();
    for event in model.values() {
        let entry = by_day.entry(copied(event.day.as_ref())).or_default();
        entry.0 += 1;
        entry.1 += event.amount;
    }
    let expected = sorted(
        by_day
            .iter()
            .map(|(day, (count, sum))| {
                vec![text(day.as_deref()), count.to_string(), sum.to_string()]
            })
            .collect(),
    );
    assert_eq!(
        run(
            events,
            "SELECT day, COUNT(*), SUM(amount) FROM events GROUP BY day"
        ),
        expected,
        "GROUP BY a DATE"
    );

    let mut positive = BTreeMap::<Option<String>, u64>::new();
    for event in model.values().filter(|event| event.amount > 0) {
        *positive.entry(copied(event.day.as_ref())).or_default() += 1;
    }
    assert_eq!(
        run(
            events,
            "SELECT day, COUNT(*) FROM (SELECT day, amount FROM events WHERE amount > 0) d \
             GROUP BY day"
        ),
        sorted(
            positive
                .iter()
                .map(|(day, count)| vec![text(day.as_deref()), count.to_string()])
                .collect()
        ),
        "GROUP BY a derived DATE"
    );

    let mut by_date = BTreeMap::<Option<String>, u64>::new();
    for event in model.values() {
        *by_date
            .entry(event.at.as_ref().map(|at| at[..10].to_owned()))
            .or_default() += 1;
    }
    assert_eq!(
        run(
            events,
            "SELECT DATE(at), COUNT(*) FROM events GROUP BY DATE(at)"
        ),
        sorted(
            by_date
                .iter()
                .map(|(day, count)| vec![text(day.as_deref()), count.to_string()])
                .collect()
        ),
        "GROUP BY DATE() of a DATETIME"
    );

    let distinct = model
        .values()
        .filter_map(|event| copied(event.day.as_ref()))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        run(events, "SELECT COUNT(DISTINCT day) FROM events"),
        vec![vec![distinct.len().to_string()]],
        "COUNT(DISTINCT) of a DATE"
    );

    let days = model.values().filter_map(|event| event.day.clone());
    let ats = model.values().filter_map(|event| event.at.clone());
    let (least_day, greatest_day) = (days.clone().min(), days.max());
    let (least_at, greatest_at) = (ats.clone().min(), ats.max());
    assert_eq!(
        run(
            events,
            "SELECT MIN(day), MAX(day), MIN(at), MAX(at) FROM events"
        ),
        vec![vec![
            text(least_day.as_deref()),
            text(greatest_day.as_deref()),
            text(least_at.as_deref()),
            text(greatest_at.as_deref()),
        ]],
        "MIN and MAX"
    );

    let mut by_month = BTreeMap::<(String, String), u64>::new();
    for event in model.values() {
        let part = |range: std::ops::Range<usize>| {
            event.day.as_ref().map_or_else(
                || "NULL".to_owned(),
                |day| day[range].parse::<u32>().expect("digits").to_string(),
            )
        };
        *by_month.entry((part(0..4), part(5..7))).or_default() += 1;
    }
    assert_eq!(
        run(
            events,
            "SELECT YEAR(day), MONTH(day), COUNT(*) FROM events GROUP BY YEAR(day), MONTH(day)"
        ),
        sorted(
            by_month
                .iter()
                .map(|((year, month), count)| vec![year.clone(), month.clone(), count.to_string()])
                .collect()
        ),
        "GROUP BY YEAR() and MONTH()"
    );

    let early = model
        .values()
        .filter(|event| event.day.as_deref().is_some_and(|day| day < "2020-01-20"))
        .count();
    assert_eq!(
        run(
            events,
            "SELECT COUNT(*) FROM events WHERE day < '2020-01-20'"
        ),
        vec![vec![early.to_string()]],
        "a range that holds the zero date"
    );
}

#[test]
fn groupings_over_zero_dates_answer_as_the_model_does() {
    for layout in [
        Layout::Settled,
        Layout::MemtableOnly,
        Layout::SettledWithPartialWrites,
    ] {
        let events = events(layout, 50_000, 40);
        check(&events);
    }
}

/// Keys over more days than a small-group fold takes, which fold by their
/// range of days with the zero date in a slot of its own, answer as the
/// model does, in every layout.
#[test]
fn a_range_of_days_with_zero_dates_answers_as_the_model_does() {
    for layout in [
        Layout::Settled,
        Layout::MemtableOnly,
        Layout::SettledWithPartialWrites,
    ] {
        check(&events(layout, 300_000, 1_500));
    }
}

/// A segment holding zero dates decodes its calendar columns packed, with
/// text derived from the units, so the grouping keys by units.
#[test]
fn a_segment_holding_zero_dates_scans_packed() {
    let events = events(Layout::Settled, 30_000, 40);
    for sql in [
        "SELECT day, COUNT(*) FROM events GROUP BY day",
        "SELECT DATE(at), COUNT(*) FROM events GROUP BY DATE(at)",
    ] {
        let (_, notes) = pull(&events, sql);
        assert!(
            notes.contains("small-group column fold") && notes.contains(" 0 per row"),
            "{sql}: every batch folds by column: {notes}"
        );
    }
}
