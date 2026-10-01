//! What a selective filter over a wide table pays for the columns it does
//! not test.
//!
//! `#[ignore]`: measurement, not assertion. Run with
//! `PINTAIL_DISABLE_SETTLED_MEMO=1 cargo test --profile recovery -p
//! pintail-exec --test integration late_materialization_cost:: -- --ignored
//! --nocapture`. `LATE_ROWS` sets the table size (default 20,000,000),
//! `LATE_RUNS` the timed repetitions, `LATE_ONLY` a label substring, and
//! `LATE_PROFILE=1` beside `PINTAIL_PROFILE=1` prints each case's profile.
//!
//! The table is invented: twelve columns of integers, decimals, temporals
//! and text, with a selector column spread pseudo-randomly over the key so
//! `sel < n` keeps `n` rows in ten thousand, scattered through every block.
//! No block statistic can skip anything; only the selection can. Every
//! answer is checked against a direct computation over the generator.
//!
//! Two more columns change their selectivity along the key, so a filter on
//! them keeps nearly every row of some segments and nearly none of others:
//! `status` is 1 for 99 rows in a hundred of the first half of the table
//! and for one in a hundred of the second, and `seen` rises with the key
//! (with jitter, and NULL once in 29 rows), so a window over it is all of
//! the early segments, none of the late ones and a ragged edge between.
//! `band` is 1 for every row of the first half and, in the second, for the
//! first thousand rows of each hundred thousand: one unbroken run a
//! segment rather than scattered rows. The `phased` cases measure what a
//! scan pays when the share it keeps moves under it, beside one filter
//! that keeps nearly everything throughout.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const LABELS: [&str; 5] = ["amber", "beige", "coral", "denim", "ebony"];
const CHUNK: u64 = 100_000;
const THRESHOLDS: [u64; 4] = [1, 100, 1000, 5000];

fn schema() -> TableSchema {
    let decimal = |precision, scale| DataType::Decimal { precision, scale };
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "sel", DataType::Int64, false),
            Column::new(3, "grp", DataType::UInt32, false),
            Column::new(4, "qty", DataType::Int64, false),
            Column::new(5, "amount", decimal(12, 2), false),
            Column::new(6, "price", decimal(10, 2), false),
            Column::new(7, "created", DataType::DateTime64 { fsp: 0 }, false),
            Column::new(8, "shipped", DataType::Date32, true),
            Column::new(9, "label", DataType::Utf8, false),
            Column::new(10, "note", DataType::Utf8, false),
            Column::new(11, "city", DataType::Utf8, false),
            Column::new(12, "score", DataType::Int64, true),
            Column::new(13, "status", DataType::Int64, false),
            Column::new(14, "seen", DataType::Int64, true),
            Column::new(15, "band", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn mix(id: u64) -> u64 {
    let mut z = id.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One generated row, in plain Rust terms.
struct Gen {
    sel: u64,
    qty: u64,
    amount: u64,
    price: u64,
    created: String,
    shipped: Option<String>,
    label: usize,
    note: String,
    city: String,
    score: Option<u64>,
}

fn civil(days_since_2020: u64) -> (u64, u64, u64) {
    let days = 18_262 + i64::try_from(days_since_2020).expect("small");
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        u64::try_from(y).expect("year"),
        u64::try_from(m).expect("month"),
        u64::try_from(d).expect("day"),
    )
}

fn generate(id: u64) -> Gen {
    let h = mix(id);
    let day = (id * 13) % 1825;
    let (y, m, d) = civil(day);
    let second = (h >> 20) % 86_400;
    let (sy, sm, sd) = civil((day + 3) % 1825);
    Gen {
        sel: h % 10_000,
        qty: 1 + (h >> 14) % 50,
        amount: 100 + (h >> 24) % 9_999_900,
        price: 1 + (id * 7919) % 99_999,
        created: format!(
            "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
            second / 3600,
            second / 60 % 60,
            second % 60
        ),
        shipped: (!id.is_multiple_of(7)).then(|| format!("{sy:04}-{sm:02}-{sd:02}")),
        label: usize::try_from(id % 5).expect("small"),
        note: format!("n{h:016x}{id:08x}"),
        city: format!("c{:03}", (h >> 40) % 211),
        score: (!id.is_multiple_of(11)).then_some((h >> 33) % 1000),
    }
}

/// `status` of row `id` in a table of `rows`: mostly 1 in the first half,
/// mostly 0 in the second.
fn status(id: u64, rows: u64) -> u64 {
    let flip = (mix(id) >> 50) % 100;
    u64::from(if id <= rows / 2 { flip < 99 } else { flip < 1 })
}

/// `seen` of row `id`: the key plus a jitter of up to a twentieth of the
/// table, NULL once in 29 rows.
fn seen(id: u64, rows: u64) -> Option<u64> {
    (!id.is_multiple_of(29)).then(|| id + (mix(id) >> 45) % (rows / 20).max(1))
}

/// `band` of row `id`: 1 through the first half, then 1 for the first
/// thousand rows of each hundred thousand.
fn band(id: u64, rows: u64) -> u64 {
    u64::from(id <= rows / 2 || (id - 1) % CHUNK < 1000)
}

fn cents(value: u64) -> String {
    format!("{}.{:02}", value / 100, value % 100)
}

fn row(id: u64, rows: u64) -> StoredRow {
    let g = generate(id);
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(i64::try_from(g.sel).expect("small")),
            Value::UInt64(id % 1000),
            Value::Int64(i64::try_from(g.qty).expect("small")),
            Value::Utf8(cents(g.amount)),
            Value::Utf8(cents(g.price)),
            Value::Utf8(g.created),
            g.shipped.map_or(Value::Null, Value::Utf8),
            Value::Utf8(LABELS[g.label].to_owned()),
            Value::Utf8(g.note),
            Value::Utf8(g.city),
            g.score.map_or(Value::Null, |score| {
                Value::Int64(i64::try_from(score).expect("small"))
            }),
            Value::Int64(i64::try_from(status(id, rows)).expect("small")),
            seen(id, rows).map_or(Value::Null, |seen| {
                Value::Int64(i64::try_from(seen).expect("small"))
            }),
            Value::Int64(i64::try_from(band(id, rows)).expect("small")),
        ],
        id + 1,
        false,
    )
}

#[derive(Default)]
struct Totals {
    count: u64,
    amount: u64,
    qty: u64,
    created: Option<String>,
    note: Option<String>,
    city: Option<String>,
    score: Option<u64>,
}

#[derive(Default)]
struct Grouped {
    count: u64,
    price: u64,
    shipped: Option<String>,
    note: Option<String>,
}

fn keep_max(slot: &mut Option<String>, value: &str) {
    if slot.as_deref().is_none_or(|current| value > current) {
        *slot = Some(value.to_owned());
    }
}

fn keep_min(slot: &mut Option<String>, value: &str) {
    if slot.as_deref().is_none_or(|current| value < current) {
        *slot = Some(value.to_owned());
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new(rows: u64) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut table =
            TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
        let mut start = 1;
        while start <= rows {
            let end = (start + CHUNK).min(rows + 1);
            table
                .bulk_ingest_snapshot((start..end).map(|id| row(id, rows)).collect())
                .expect("rows");
            start = end;
        }
        let entry = TableEntry::new(
            TableId::new(1),
            "wide",
            schema(),
            TableStatistics::with_row_count(rows),
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

    fn run(&self, sql: &str, profile: bool) -> (Vec<String>, f64, String) {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let started = std::time::Instant::now();
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 4 << 30, Collation::default()).expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                let values: Vec<String> = batch
                    .columns()
                    .iter()
                    .map(|column| match column.value_owned(row).expect("value") {
                        Value::Int64(value) => value.to_string(),
                        Value::UInt64(value) => value.to_string(),
                        Value::Utf8(text) => text,
                        Value::Null => "NULL".to_owned(),
                        other => format!("{other:?}"),
                    })
                    .collect();
                rows.push(values.join("|"));
            }
        }
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        if profile && let Some(profile) = execution.profile() {
            println!("{}", profile.render());
        }
        let stats = provider
            .scan_stats(DatabaseId::new(1), TableId::new(1))
            .map(|stats| {
                format!(
                    "blocks {}/{} decoded {} bytes_decompressed {} values_decoded {}",
                    stats.blocks_read,
                    stats.blocks_total(),
                    stats.blocks_decoded,
                    stats.bytes_decompressed,
                    stats.values_decoded
                )
            })
            .unwrap_or_default();
        (rows, elapsed, stats)
    }
}

/// Per threshold: the totals row, the grouped rows and the projected rows.
type Answers = (Vec<String>, Vec<String>, Vec<String>);

/// Expected answers for every threshold, from one pass over the generator.
fn expected(rows: u64) -> BTreeMap<u64, Answers> {
    let mut totals: Vec<Totals> = THRESHOLDS.iter().map(|_| Totals::default()).collect();
    let mut groups: Vec<BTreeMap<usize, Grouped>> =
        THRESHOLDS.iter().map(|_| BTreeMap::new()).collect();
    let mut points = Vec::new();
    for id in 1..=rows {
        let h = mix(id) % 10_000;
        if h >= THRESHOLDS[THRESHOLDS.len() - 1] {
            continue;
        }
        let g = generate(id);
        if g.sel < THRESHOLDS[0] {
            points.push(format!("{id}|{}|{}|{}", g.note, cents(g.amount), g.created));
        }
        for (index, threshold) in THRESHOLDS.iter().enumerate() {
            if g.sel >= *threshold {
                continue;
            }
            let t = &mut totals[index];
            t.count += 1;
            t.amount += g.amount;
            t.qty += g.qty;
            keep_max(&mut t.created, &g.created);
            keep_min(&mut t.note, &g.note);
            keep_max(&mut t.city, &g.city);
            if let Some(score) = g.score {
                *t.score.get_or_insert(0) += score;
            }
            let entry = groups[index].entry(g.label).or_default();
            entry.count += 1;
            entry.price += g.price;
            if let Some(shipped) = &g.shipped {
                keep_max(&mut entry.shipped, shipped);
            }
            keep_max(&mut entry.note, &g.note);
        }
    }
    let text = |value: &Option<String>| value.clone().unwrap_or_else(|| "NULL".to_owned());
    THRESHOLDS
        .iter()
        .enumerate()
        .map(|(index, threshold)| {
            let t = &totals[index];
            let total = vec![format!(
                "{}|{}|{}|{}|{}|{}|{}",
                t.count,
                cents(t.amount),
                t.qty,
                text(&t.created),
                text(&t.note),
                text(&t.city),
                t.score
                    .map_or_else(|| "NULL".to_owned(), |score| score.to_string())
            )];
            let grouped = groups[index]
                .iter()
                .map(|(label, g)| {
                    format!(
                        "{}|{}|{}|{}|{}",
                        LABELS[*label],
                        g.count,
                        cents(g.price),
                        text(&g.shipped),
                        text(&g.note)
                    )
                })
                .collect();
            let points = if index == 0 {
                points.clone()
            } else {
                Vec::new()
            };
            (*threshold, (total, grouped, points))
        })
        .collect()
}

/// A label, the filter as SQL, and the same test over a row's key.
type Phase = (&'static str, String, Box<dyn Fn(u64) -> bool>);

/// The cases whose filter keeps a different share of each segment, with
/// their answers from one pass over the generator.
fn phased(rows: u64) -> Vec<(String, String, Vec<String>)> {
    let (low, high) = (rows / 3, rows * 2 / 3);
    let filters: [Phase; 7] = [
        (
            "phased dense>runs",
            "band = 1".to_owned(),
            Box::new(move |id| band(id, rows) == 1),
        ),
        (
            "phased dense>sparse",
            "status = 1".to_owned(),
            Box::new(move |id| status(id, rows) == 1),
        ),
        (
            "phased sparse>dense",
            "status = 0".to_owned(),
            Box::new(move |id| status(id, rows) == 0),
        ),
        (
            "phased window head",
            format!("seen < {}", rows / 2),
            Box::new(move |id| seen(id, rows).is_some_and(|seen| seen < rows / 2)),
        ),
        (
            "phased window mid",
            format!("seen >= {low} AND seen < {high}"),
            Box::new(move |id| seen(id, rows).is_some_and(|seen| seen >= low && seen < high)),
        ),
        (
            "phased window tail",
            format!("seen >= {}", rows / 2),
            Box::new(move |id| seen(id, rows).is_some_and(|seen| seen >= rows / 2)),
        ),
        (
            "phased dense all",
            "sel < 9500".to_owned(),
            Box::new(|id| mix(id) % 10_000 < 9500),
        ),
    ];
    let mut totals: Vec<Totals> = filters.iter().map(|_| Totals::default()).collect();
    for id in 1..=rows {
        let mut generated = None;
        for (index, (_, _, keep)) in filters.iter().enumerate() {
            if !keep(id) {
                continue;
            }
            let g = generated.get_or_insert_with(|| generate(id));
            let t = &mut totals[index];
            t.count += 1;
            t.amount += g.amount;
            t.qty += g.qty;
            keep_min(&mut t.note, &g.note);
            keep_max(&mut t.city, &g.city);
            if let Some(score) = g.score {
                *t.score.get_or_insert(0) += score;
            }
        }
    }
    let text = |value: &Option<String>| value.clone().unwrap_or_else(|| "NULL".to_owned());
    filters
        .iter()
        .zip(&totals)
        .map(|((label, filter, _), t)| {
            (
                (*label).to_owned(),
                format!(
                    "SELECT COUNT(*), SUM(amount), SUM(qty), MIN(note), MAX(city), SUM(score) \
                     FROM wide WHERE {filter}"
                ),
                vec![format!(
                    "{}|{}|{}|{}|{}|{}",
                    t.count,
                    cents(t.amount),
                    t.qty,
                    text(&t.note),
                    text(&t.city),
                    t.score
                        .map_or_else(|| "NULL".to_owned(), |score| score.to_string())
                )],
            )
        })
        .collect()
}

/// Lean aggregates whose cost is the fold itself, one per way the fold
/// meets its rows: every row of a batch with no NULL among them, every row
/// of nullable columns, and half the rows picked by a filter. The answers
/// come from the generator's own arithmetic, without building its text.
fn folds(rows: u64) -> Vec<(String, String, Vec<String>)> {
    let (mut amount, mut price) = (0_u64, 0_u64);
    let (mut scored, mut score, mut shipped) = (0_u64, 0_u64, 0_u64);
    let (mut picked, mut picked_amount, mut picked_price) = (0_u64, 0_u64, 0_u64);
    for id in 1..=rows {
        let h = mix(id);
        let row_amount = 100 + (h >> 24) % 9_999_900;
        let row_price = 1 + (id * 7919) % 99_999;
        amount += row_amount;
        price += row_price;
        if !id.is_multiple_of(11) {
            scored += 1;
            score += (h >> 33) % 1000;
        }
        shipped += u64::from(!id.is_multiple_of(7));
        if h % 10_000 < 5000 {
            picked += 1;
            picked_amount += row_amount;
            picked_price += row_price;
        }
    }
    vec![
        (
            "fold whole run".to_owned(),
            "SELECT COUNT(*), SUM(amount), SUM(price) FROM wide WHERE qty > 0".to_owned(),
            vec![format!("{rows}|{}|{}", cents(amount), cents(price))],
        ),
        (
            "fold nullable".to_owned(),
            "SELECT COUNT(score), SUM(score), COUNT(shipped) FROM wide WHERE qty > 0".to_owned(),
            vec![format!("{scored}|{score}|{shipped}")],
        ),
        (
            "fold picked half".to_owned(),
            "SELECT COUNT(*), SUM(amount), SUM(price) FROM wide WHERE sel < 5000".to_owned(),
            vec![format!(
                "{picked}|{}|{}",
                cents(picked_amount),
                cents(picked_price)
            )],
        ),
    ]
}

#[test]
#[ignore = "measurement, not an assertion"]
fn late_materialization_cost() {
    let rows = std::env::var("LATE_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20_000_000_u64);
    let runs = std::env::var("LATE_RUNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(11_usize);
    let only = std::env::var("LATE_ONLY").ok();
    let profile = std::env::var_os("LATE_PROFILE").is_some();
    let built = std::time::Instant::now();
    let fixture = Fixture::new(rows);
    let expected = expected(rows);
    println!(
        "fixture: {rows} rows in {:.1}s",
        built.elapsed().as_secs_f64()
    );
    let mut cases: Vec<(String, String, Vec<String>)> = Vec::new();
    for (threshold, (total, grouped, points)) in &expected {
        #[allow(clippy::cast_precision_loss)]
        let percent = *threshold as f64 / 100.0;
        cases.push((
            format!("totals {percent}%"),
            format!(
                "SELECT COUNT(*), SUM(amount), SUM(qty), MAX(created), MIN(note), MAX(city), \
                 SUM(score) FROM wide WHERE sel < {threshold}"
            ),
            total.clone(),
        ));
        cases.push((
            format!("grouped {percent}%"),
            format!(
                "SELECT label, COUNT(*), SUM(price), MAX(shipped), MAX(note) FROM wide \
                 WHERE sel < {threshold} GROUP BY label ORDER BY label"
            ),
            grouped.clone(),
        ));
        if !points.is_empty() {
            cases.push((
                format!("rows {percent}%"),
                format!(
                    "SELECT id, note, amount, created FROM wide WHERE sel < {threshold} \
                     ORDER BY id"
                ),
                points.clone(),
            ));
        }
    }
    // Their labels all start with the word, and their expected answers
    // cost a pass over the generator.
    if only.as_ref().is_none_or(|only| only.contains("phased")) {
        cases.extend(phased(rows));
    }
    if only.as_ref().is_none_or(|only| only.contains("fold")) {
        cases.extend(folds(rows));
    }
    for (label, sql, expected) in &cases {
        // `LATE_ONLY` may name several substrings, separated by commas.
        if only
            .as_ref()
            .is_some_and(|only| !only.split(',').any(|part| label.contains(part)))
        {
            continue;
        }
        let (answer, _, stats) = fixture.run(sql, profile);
        assert_eq!(&answer, expected, "{label}: answer differs");
        let mut times: Vec<f64> = (0..runs).map(|_| fixture.run(sql, false).1).collect();
        times.sort_by(f64::total_cmp);
        println!(
            "{label:<20} median {:>8.2}ms  min {:>8.2}ms  {stats}",
            times[runs / 2],
            times[0]
        );
    }
}
