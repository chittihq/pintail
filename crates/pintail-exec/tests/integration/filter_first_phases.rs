//! A filter keeps a different share of each stretch of a table: nearly
//! every row of some segments, nearly none of others. The filter-first
//! scan decodes the dense stretches whole, judging only a sample of their
//! chunks, and goes back to selecting rows where the sample starts
//! rejecting them. These tests hold every answer to a direct computation
//! over a model of the table while the share changes four times along the
//! key, across NULLs in the tested and the read columns, live memtable
//! rows over the segments, and a column added by a schema change that
//! older segments lack.
//!
//! The table is invented: a key, a status that is mostly one value or
//! mostly the other by quarter of the key range, a nullable value that
//! rises with the key, a decimal, a nullable text and a nullable integer,
//! later joined by a nullable integer added with the schema. It is written
//! as many small segments, so a scan reads it in several rounds.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const SEGMENTS: u64 = 400;
const SEGMENT_ROWS: u64 = 250;
const ROWS: u64 = SEGMENTS * SEGMENT_ROWS;

fn schema(with_extra: bool) -> TableSchema {
    let mut columns = vec![
        Column::new(1, "id", DataType::UInt64, false),
        Column::new(2, "status", DataType::Int64, false),
        Column::new(3, "seen", DataType::Int64, true),
        Column::new(
            4,
            "amount",
            DataType::Decimal {
                precision: 12,
                scale: 2,
            },
            false,
        ),
        Column::new(5, "note", DataType::Utf8, true),
        Column::new(6, "score", DataType::Int64, true),
    ];
    if with_extra {
        columns.push(Column::new(7, "extra", DataType::Int64, true));
    }
    TableSchema::new(if with_extra { 2 } else { 1 }, columns).expect("schema")
}

fn mix(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[derive(Clone)]
struct Model {
    status: i64,
    seen: Option<i64>,
    cents: u64,
    note: Option<String>,
    score: Option<i64>,
    extra: Option<i64>,
}

/// Row `id`. Its status is 1 for 99 rows in a hundred of the first quarter
/// of the key range, two in a hundred of the second, every row of the
/// third and one in a hundred of the last; `salt` redraws the row's values.
fn generate(id: u64, salt: u64) -> Model {
    let h = mix(id ^ (salt << 40));
    let ones_in_hundred = match (id.saturating_sub(1)) * 4 / ROWS {
        0 => 99,
        1 => 2,
        2 => 100,
        _ => 1,
    };
    Model {
        status: i64::from((h >> 52) % 100 < ones_in_hundred),
        seen: (!id.is_multiple_of(29))
            .then(|| i64::try_from(id + (h >> 44) % (ROWS / 20)).expect("small")),
        cents: 1 + (h >> 12) % 100_000,
        note: (!id.is_multiple_of(17)).then(|| format!("n{:012x}", (h >> 16) & 0xFFFF_FFFF_FFFF)),
        score: (!id.is_multiple_of(11)).then(|| i64::try_from((h >> 30) % 500).expect("small")),
        extra: None,
    }
}

fn stored(id: u64, model: &Model, version: u64, with_extra: bool, deleted: bool) -> StoredRow {
    let mut values = vec![
        Value::UInt64(id),
        Value::Int64(model.status),
        model.seen.map_or(Value::Null, Value::Int64),
        Value::Utf8(cents(model.cents)),
        model.note.clone().map_or(Value::Null, Value::Utf8),
        model.score.map_or(Value::Null, Value::Int64),
    ];
    if with_extra {
        values.push(model.extra.map_or(Value::Null, Value::Int64));
    }
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        values,
        version,
        deleted,
    )
}

fn cents(value: u64) -> String {
    format!("{}.{:02}", value / 100, value % 100)
}

fn run(table: &TableStore, schema: &TableSchema, sql: &str) -> Vec<String> {
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TableId::new(1),
        "events",
        schema.clone(),
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 1 << 30, Collation::default()).expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("batch") {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value_owned(row).expect("value") {
                        Value::Int64(value) => value.to_string(),
                        Value::UInt64(value) => value.to_string(),
                        Value::Utf8(text) => text,
                        Value::Null => "NULL".to_owned(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
    }
    rows
}

fn text(value: Option<&str>) -> String {
    value.map_or_else(|| "NULL".to_owned(), str::to_owned)
}

fn number(value: Option<i64>) -> String {
    value.map_or_else(|| "NULL".to_owned(), |value| value.to_string())
}

/// A filter as SQL and as the same test over the model.
type Filter = (String, Box<dyn Fn(&Model) -> bool>);

fn filters(with_extra: bool) -> Vec<Filter> {
    let rows = i64::try_from(ROWS).expect("small");
    let (low, high) = (rows * 3 / 8, rows * 5 / 8);
    let mut filters: Vec<Filter> = vec![
        ("status = 1".to_owned(), Box::new(|row| row.status == 1)),
        ("status = 0".to_owned(), Box::new(|row| row.status == 0)),
        (
            format!("seen < {}", rows / 2),
            Box::new(move |row| row.seen.is_some_and(|seen| seen < rows / 2)),
        ),
        (
            format!("seen >= {low} AND seen < {high}"),
            Box::new(move |row| row.seen.is_some_and(|seen| seen >= low && seen < high)),
        ),
        (
            format!("seen >= {}", rows / 2),
            Box::new(move |row| row.seen.is_some_and(|seen| seen >= rows / 2)),
        ),
        (
            format!("status = 1 AND seen >= {low}"),
            Box::new(move |row| row.status == 1 && row.seen.is_some_and(|seen| seen >= low)),
        ),
        (
            "seen IS NULL".to_owned(),
            Box::new(|row| row.seen.is_none()),
        ),
    ];
    if with_extra {
        filters.push((
            "extra < 50".to_owned(),
            Box::new(|row| row.extra.is_some_and(|extra| extra < 50)),
        ));
        filters.push((
            "extra IS NULL".to_owned(),
            Box::new(|row| row.extra.is_none()),
        ));
    }
    filters
}

/// Every filter, as totals and as the rows themselves, against the model.
fn check(table: &TableStore, schema: &TableSchema, model: &BTreeMap<u64, Model>, stage: &str) {
    let with_extra = schema.columns().len() == 7;
    for (filter, keep) in filters(with_extra) {
        let kept = model
            .iter()
            .filter(|(_, row)| keep(row))
            .collect::<Vec<_>>();
        let count = kept.len();
        let amount: u64 = kept.iter().map(|(_, row)| row.cents).sum();
        let notes = kept.iter().filter_map(|(_, row)| row.note.as_deref());
        let scores = kept.iter().filter_map(|(_, row)| row.score);
        let score_sum = (scores.clone().count() > 0).then(|| scores.clone().sum::<i64>());
        let expected = vec![format!(
            "{count}|{}|{}|{}|{}|{}",
            if count == 0 {
                "NULL".to_owned()
            } else {
                cents(amount)
            },
            text(notes.clone().min()),
            text(notes.max()),
            number(score_sum),
            scores.count()
        )];
        let sql = format!(
            "SELECT COUNT(*), SUM(amount), MIN(note), MAX(note), SUM(score), COUNT(score) \
             FROM events WHERE {filter}"
        );
        assert_eq!(run(table, schema, &sql), expected, "{stage}: {sql}");

        let expected = kept
            .iter()
            .map(|(id, row)| {
                let mut line = format!(
                    "{id}|{}|{}|{}|{}",
                    number(row.seen),
                    text(row.note.as_deref()),
                    cents(row.cents),
                    number(row.score)
                );
                if with_extra {
                    line.push('|');
                    line.push_str(&number(row.extra));
                }
                line
            })
            .collect::<Vec<_>>();
        let sql = format!(
            "SELECT id, seen, note, amount, score{} FROM events WHERE {filter} ORDER BY id",
            if with_extra { ", extra" } else { "" }
        );
        assert_eq!(run(table, schema, &sql), expected, "{stage}: {sql}");
    }
}

#[test]
fn a_filter_whose_selectivity_changes_along_the_key_matches_the_model() {
    let directory = tempfile::tempdir().expect("directory");
    let v1 = schema(false);
    let mut table =
        TableStore::open(directory.path(), v1.clone(), StoreOptions::default()).expect("table");
    let mut model: BTreeMap<u64, Model> = BTreeMap::new();
    for segment in 0..SEGMENTS {
        let rows = (segment * SEGMENT_ROWS + 1..=(segment + 1) * SEGMENT_ROWS)
            .map(|id| {
                let row = generate(id, 0);
                let stored = stored(id, &row, id, false, false);
                model.insert(id, row);
                stored
            })
            .collect();
        table.bulk_ingest_snapshot(rows).expect("snapshot");
    }
    check(&table, &v1, &model, "segments");

    // Live rows over the segments: updates that redraw the status, deletes
    // and appends.
    let mut version = ROWS + 1;
    let mut live = Vec::new();
    for id in (1..=ROWS).step_by(97) {
        let mut row = generate(id, 1);
        row.status = 1 - row.status;
        live.push(stored(id, &row, version, false, false));
        model.insert(id, row);
        version += 1;
    }
    for id in (5..=ROWS).step_by(101) {
        let row = model.remove(&id).unwrap_or_else(|| generate(id, 0));
        live.push(stored(id, &row, version, false, true));
        version += 1;
    }
    for id in ROWS + 1..=ROWS + 500 {
        let row = generate(id, 2);
        live.push(stored(id, &row, version, false, false));
        model.insert(id, row);
        version += 1;
    }
    table.ingest_cdc(live).expect("live rows");
    check(&table, &v1, &model, "memtable");

    // A nullable column added: every row so far reads it as NULL.
    let v2 = schema(true);
    table.evolve_schema(v2.clone()).expect("evolve");
    check(&table, &v2, &model, "after the schema change");

    // Rows written after the change carry it, in the memtable and then in
    // a segment of their own beside the older ones. They sit in the second
    // half of the key range, so a filter on the new column keeps nothing
    // of the first half and a tenth of the second.
    let mut live = Vec::new();
    for id in (ROWS / 2..=ROWS + 500).step_by(5) {
        let Some(row) = model.get_mut(&id) else {
            continue;
        };
        row.extra = Some(i64::try_from(mix(id) % 100).expect("small"));
        live.push(stored(id, row, version, true, false));
        version += 1;
    }
    table.ingest_cdc(live).expect("live rows");
    check(&table, &v2, &model, "memtable after the schema change");
    table.flush().expect("flush");
    check(&table, &v2, &model, "flushed after the schema change");
}
