//! Text columns holding a few fixed values - a state, a kind, a country -
//! written as VARCHAR rather than ENUM. Their blocks are coded against one
//! dictionary per column of a segment, and a filter on such a column skips
//! the blocks that hold no value it accepts.
//!
//! The answers are checked against a model computed here from the rows:
//! filters by equality, lists, negation, patterns and NULL tests under a
//! collation that ignores case; groups, which show the spelling of their
//! first row; with the rows settled, and with updates, deletes and new rows
//! in the memtable over them.
//!
//! The ignored bench prints what each shape costs on an invented table:
//! `cargo test --profile recovery -p pintail-exec --test integration
//! low_cardinality_text::bench -- --ignored --nocapture`. `BENCH_ROWS` sets
//! the table size (4M), `BENCH_RUNS` the repeats (9) and `BENCH_SQL` the
//! statements to time instead, separated by `;`.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Three states, the first under three spellings the default collation
/// calls equal.
const STATES: [&str; 5] = ["active", "Active", "ACTIVE", "closed", "pending"];
const KINDS: [&str; 8] = [
    "invoice", "refund", "credit", "debit", "transfer", "fee", "bonus", "void",
];

fn mix(id: u64) -> u64 {
    let mut x = id.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn pick(seed: u64, modulus: u64) -> usize {
    usize::try_from(seed % modulus).expect("small")
}

/// A skewed choice among `count` values: low indices far more often.
fn skewed(seed: u64, count: u64) -> u64 {
    let a = seed % count;
    let b = (seed >> 20) % count;
    let c = (seed >> 40) % count;
    a.min(b).min(c)
}

#[derive(Clone, Debug)]
struct Entry {
    state: String,
    kind: String,
    country: String,
    plan: String,
    tag: String,
    /// NULL in one row of nine; `flagged` only in two short id spans.
    mark: Option<String>,
    qty: i64,
}

/// The row `id` holds at `version`, in a table of `rows` rows.
fn entry(id: u64, version: u64, rows: u64) -> Entry {
    let h = mix(id ^ (version << 48));
    let state = match h % 100 {
        0..=69 => pick(h >> 8, 3),
        70..=94 => 3,
        _ => 4,
    };
    let flagged = version == 1
        && ((rows / 3..rows / 3 + 40).contains(&id) || (rows - 500..rows - 480).contains(&id));
    Entry {
        state: STATES[state].to_owned(),
        kind: KINDS[usize::try_from(skewed(h >> 3, 8)).expect("small")].to_owned(),
        country: format!("C{:02}", skewed(h >> 5, 40)),
        plan: format!("plan-{:03}", skewed(h >> 7, 200)),
        tag: format!("tag-{:04}", (h >> 9) % 5000),
        mark: if flagged {
            Some("flagged".to_owned())
        } else if mix(h ^ 9).is_multiple_of(9) {
            None
        } else {
            Some("normal".to_owned())
        },
        qty: i64::try_from(h % 1000).expect("small") - 200,
    }
}

fn stored(id: u64, entry: &Entry, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(entry.state.clone()),
            Value::Utf8(entry.kind.clone()),
            Value::Utf8(entry.country.clone()),
            Value::Utf8(entry.plan.clone()),
            Value::Utf8(entry.tag.clone()),
            entry.mark.clone().map_or(Value::Null, Value::Utf8),
            Value::Int64(entry.qty),
        ],
        version,
        deleted,
    )
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "state", DataType::Utf8, false),
            Column::new(3, "kind", DataType::Utf8, false),
            Column::new(4, "country", DataType::Utf8, false),
            Column::new(5, "plan", DataType::Utf8, false),
            Column::new(6, "tag", DataType::Utf8, false),
            Column::new(7, "mark", DataType::Utf8, true),
            Column::new(8, "qty", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

type Model = BTreeMap<u64, Entry>;

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
    model: Model,
}

/// `rows` settled rows; with `writes`, every seventh updated, some deleted
/// and a few appended, all still in the memtable. `keep_model` is off for
/// the bench, which only times.
fn fixture(rows: u64, writes: bool, keep_model: bool) -> Fixture {
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
    let mut start = 1;
    while start <= rows {
        let end = (start + 100_000).min(rows + 1);
        let mut chunk = Vec::new();
        for id in start..end {
            let entry = entry(id, 1, rows);
            chunk.push(stored(id, &entry, 1, false));
            if keep_model {
                model.insert(id, entry);
            }
        }
        table.bulk_ingest_snapshot(chunk).expect("ingest");
        start = end;
    }
    if writes {
        let mut changed = Vec::new();
        for id in (1..=rows).step_by(7) {
            let entry = entry(id, 3, rows);
            changed.push(stored(id, &entry, 3, false));
            model.insert(id, entry);
        }
        for id in (3..=rows).step_by(31) {
            if let Some(entry) = model.remove(&id) {
                changed.push(stored(id, &entry, 4, true));
            }
        }
        for id in rows + 1..=rows + 500 {
            let entry = entry(id, 3, rows);
            changed.push(stored(id, &entry, 3, false));
            model.insert(id, entry);
        }
        table.ingest(changed).expect("memtable writes");
    }
    let entry = TableEntry::new(
        TableId::new(1),
        "ledger",
        schema(),
        TableStatistics::with_row_count(rows),
    )
    .expect("entry");
    Fixture {
        _directory: directory,
        table,
        catalog: CatalogSnapshot::new([
            DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
        ])
        .expect("catalog"),
        model,
    }
}

struct Answer {
    rows: Vec<Vec<String>>,
    notes: String,
    /// Blocks the scan skipped for holding no value its filter accepts.
    skipped: usize,
    /// Slices of segments the scan read through the side index.
    index_slices: usize,
}

fn run(fixture: &Fixture, sql: &str) -> Answer {
    let snapshot = fixture.table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&fixture.catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start_profiled(plan, &provider, 8 << 30, None, Collation::default())
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
                        Some(Value::DecimalAverage(average)) => average.canonical(),
                        Some(other) => other
                            .text()
                            .map_or_else(|| format!("{other:?}"), ToOwned::to_owned),
                    })
                    .collect::<Vec<_>>(),
            );
        }
    }
    let profile = execution.profile().expect("profile");
    let stats = provider
        .scan_stats(DatabaseId::new(1), TableId::new(1))
        .unwrap_or_default();
    Answer {
        rows,
        skipped: stats.blocks_value_skipped,
        index_slices: stats.index_slices,
        notes: profile
            .operators
            .iter()
            .filter_map(|operator| operator.note.clone())
            .collect::<Vec<_>>()
            .join("; "),
    }
}

/// A filter as SQL and as the model applies it.
type Filter = (&'static str, fn(&Entry) -> bool);

fn eq(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn filters() -> Vec<Filter> {
    vec![
        ("state = 'active'", |e| eq(&e.state, "active")),
        ("state = 'CLOSED'", |e| eq(&e.state, "closed")),
        ("state = 'closed  '", |_| false),
        ("state = 'absent'", |_| false),
        ("state <> 'active'", |e| !eq(&e.state, "active")),
        ("state IN ('closed', 'Pending')", |e| {
            eq(&e.state, "closed") || eq(&e.state, "pending")
        }),
        ("state NOT IN ('closed', 'ACTIVE')", |e| {
            eq(&e.state, "pending")
        }),
        ("state LIKE 'act%'", |e| eq(&e.state, "active")),
        ("state LIKE '%END%'", |e| eq(&e.state, "pending")),
        ("mark = 'flagged'", |e| e.mark.as_deref() == Some("flagged")),
        ("mark = 'Flagged' AND qty >= 0", |e| {
            e.mark.as_deref() == Some("flagged") && e.qty >= 0
        }),
        ("mark <> 'normal'", |e| e.mark.as_deref() == Some("flagged")),
        ("mark IS NULL", |e| e.mark.is_none()),
        ("mark IS NOT NULL", |e| e.mark.is_some()),
        ("mark IN ('flagged', 'missing')", |e| {
            e.mark.as_deref() == Some("flagged")
        }),
        ("mark NOT IN ('normal')", |e| {
            e.mark.as_deref() == Some("flagged")
        }),
        ("mark = 'flagged' OR mark IS NULL", |e| {
            e.mark.as_deref() != Some("normal")
        }),
        ("kind = 'void' AND country = 'C39'", |e| {
            e.kind == "void" && e.country == "C39"
        }),
        ("plan = 'plan-199'", |e| e.plan == "plan-199"),
        ("tag = 'tag-4999'", |e| e.tag == "tag-4999"),
        ("BINARY state = 'Active'", |e| e.state == "Active"),
        ("LOWER(mark) = 'flagged'", |e| {
            e.mark.as_deref() == Some("flagged")
        }),
    ]
}

fn check_filters(fixture: &Fixture, context: &str) {
    for (sql, keeps) in filters() {
        let kept = fixture
            .model
            .iter()
            .filter(|(_, entry)| keeps(entry))
            .collect::<Vec<_>>();
        let count = run(fixture, &format!("SELECT COUNT(*) FROM ledger WHERE {sql}"));
        assert_eq!(
            count.rows,
            vec![vec![kept.len().to_string()]],
            "{context}: COUNT(*) WHERE {sql} ({})",
            count.notes
        );
        let total = run(
            fixture,
            &format!("SELECT COUNT(*), SUM(qty), MIN(id), MAX(id) FROM ledger WHERE {sql}"),
        );
        let sum = kept.iter().map(|(_, entry)| entry.qty).sum::<i64>();
        let cell = |value: Option<String>| value.unwrap_or_else(|| "NULL".to_owned());
        assert_eq!(
            total.rows,
            vec![vec![
                kept.len().to_string(),
                cell((!kept.is_empty()).then(|| sum.to_string())),
                cell(kept.first().map(|(id, _)| id.to_string())),
                cell(kept.last().map(|(id, _)| id.to_string())),
            ]],
            "{context}: totals WHERE {sql} ({})",
            total.notes
        );
    }
}

/// Groups by state as the default collation classes it, each showing the
/// spelling of the first row (in key order) that the filter keeps.
fn check_groups(fixture: &Fixture, context: &str) {
    for (sql, keeps) in [
        ("1 = 1", (|_| true) as fn(&Entry) -> bool),
        ("qty > 700", |e| e.qty > 700),
        ("mark = 'flagged'", |e| e.mark.as_deref() == Some("flagged")),
        ("state <> 'closed'", |e| !eq(&e.state, "closed")),
    ] {
        let mut expected: BTreeMap<String, (String, u64, i64, u64)> = BTreeMap::new();
        for entry in fixture.model.values().filter(|entry| keeps(entry)) {
            let group = expected
                .entry(entry.state.to_ascii_lowercase())
                .or_insert_with(|| (entry.state.clone(), 0, 0, 0));
            group.1 += 1;
            group.2 += entry.qty;
            group.3 += u64::from(entry.kind == "refund");
        }
        let mut expected = expected
            .into_values()
            .map(|(shown, count, sum, refunds)| {
                vec![
                    shown,
                    count.to_string(),
                    sum.to_string(),
                    refunds.to_string(),
                ]
            })
            .collect::<Vec<_>>();
        expected.sort();
        let answer = run(
            fixture,
            &format!(
                "SELECT state, COUNT(*), SUM(qty), SUM(kind = 'refund') FROM ledger \
                 WHERE {sql} GROUP BY state"
            ),
        );
        let mut rows = answer.rows;
        rows.sort();
        assert_eq!(rows, expected, "{context}: GROUP BY state WHERE {sql}");
    }
    let mut expected: BTreeMap<(String, String), u64> = BTreeMap::new();
    for entry in fixture.model.values() {
        *expected
            .entry((entry.state.to_ascii_lowercase(), entry.kind.clone()))
            .or_default() += 1;
    }
    let answer = run(
        fixture,
        "SELECT LOWER(state), kind, COUNT(*) FROM ledger GROUP BY state, kind",
    );
    let mut rows = answer.rows;
    rows.sort();
    let expected = expected
        .into_iter()
        .map(|((state, kind), count)| vec![state, kind, count.to_string()])
        .collect::<Vec<_>>();
    assert_eq!(rows, expected, "{context}: GROUP BY state, kind");
    for column in ["state", "kind", "country", "plan", "tag", "mark"] {
        let distinct = fixture
            .model
            .values()
            .filter_map(|entry| match column {
                "state" => Some(entry.state.to_ascii_lowercase()),
                "kind" => Some(entry.kind.clone()),
                "country" => Some(entry.country.clone()),
                "plan" => Some(entry.plan.clone()),
                "tag" => Some(entry.tag.clone()),
                _ => entry.mark.clone(),
            })
            .collect::<std::collections::BTreeSet<_>>();
        let answer = run(
            fixture,
            &format!("SELECT COUNT(DISTINCT {column}) FROM ledger"),
        );
        assert_eq!(
            answer.rows,
            vec![vec![distinct.len().to_string()]],
            "{context}: COUNT(DISTINCT {column})"
        );
    }
}

#[test]
fn filters_answer_exactly_over_settled_rows() {
    let fixture = fixture(150_000, false, true);
    check_filters(&fixture, "settled");
}

#[test]
fn filters_answer_exactly_under_writes() {
    let fixture = fixture(150_000, true, true);
    check_filters(&fixture, "settled with writes");
}

#[test]
fn groups_show_their_first_row_and_count_exactly() {
    for writes in [false, true] {
        let fixture = fixture(150_000, writes, true);
        check_groups(&fixture, if writes { "with writes" } else { "settled" });
    }
}

/// Aggregates over an expression of one text column - a comparison, a
/// CASE, a NULL test - answer as the rows do, alone and per group.
#[test]
fn conditional_aggregates_answer_exactly() {
    for writes in [false, true] {
        let fixture = fixture(150_000, writes, true);
        let count = |keeps: fn(&Entry) -> bool| {
            fixture
                .model
                .values()
                .filter(|entry| keeps(entry))
                .count()
                .to_string()
        };
        let answer = run(
            &fixture,
            "SELECT SUM(state = 'closed'), COUNT(CASE kind WHEN 'refund' THEN 1 END), \
             SUM(mark IS NULL), COUNT(IF(mark = 'Flagged', 1, NULL)), MAX(UPPER(state)), \
             SUM(CASE WHEN state LIKE 'act%' THEN 2 ELSE 0 END), COUNT(DISTINCT LOWER(state)) \
             FROM ledger",
        );
        assert_eq!(
            answer.rows,
            vec![vec![
                count(|e| eq(&e.state, "closed")),
                count(|e| e.kind == "refund"),
                count(|e| e.mark.is_none()),
                count(|e| e.mark.as_deref() == Some("flagged")),
                "PENDING".to_owned(),
                (2 * fixture
                    .model
                    .values()
                    .filter(|e| eq(&e.state, "active"))
                    .count())
                .to_string(),
                "3".to_owned(),
            ]],
            "writes={writes}: {}",
            answer.notes
        );
        let mut expected: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
        for entry in fixture.model.values() {
            let group = expected.entry(entry.kind.clone()).or_default();
            group.0 += u64::from(eq(&entry.state, "active"));
            group.1 += u64::from(entry.mark.as_deref() == Some("normal"));
            group.2 += u64::from(entry.mark.is_none());
        }
        let expected = expected
            .into_iter()
            .map(|(kind, (active, normal, nulls))| {
                vec![
                    kind,
                    active.to_string(),
                    normal.to_string(),
                    nulls.to_string(),
                ]
            })
            .collect::<Vec<_>>();
        let mut rows = run(
            &fixture,
            "SELECT kind, SUM(state = 'ACTIVE'), COUNT(CASE mark WHEN 'normal' THEN 1 END), \
             SUM(mark IS NULL) FROM ledger GROUP BY kind",
        )
        .rows;
        rows.sort();
        assert_eq!(rows, expected, "writes={writes}: per kind");
    }
}

/// A filter that only a rare value passes reads the blocks holding that
/// value alone, whatever its shape: an equality or a list of literals is
/// answered by the side index, and these are the shapes it does not take.
#[test]
fn a_rare_value_reads_only_the_blocks_holding_it() {
    let fixture = fixture(150_000, false, true);
    for sql in [
        "SELECT COUNT(*) FROM ledger WHERE mark <> 'normal'",
        "SELECT COUNT(*), SUM(qty) FROM ledger WHERE mark <> 'Normal'",
        "SELECT COUNT(*), SUM(qty) FROM ledger WHERE mark NOT IN ('normal', 'absent')",
        "SELECT id, qty FROM ledger WHERE mark LIKE 'FLAG%'",
        "SELECT id, qty FROM ledger WHERE UPPER(mark) = 'FLAGGED'",
        "SELECT COUNT(*) FROM ledger WHERE mark > 'normal' OR mark < 'g'",
    ] {
        let answer = run(&fixture, sql);
        // Eleven blocks over two segments; the value lies in two of them.
        assert_eq!(answer.skipped, 9, "{sql}: {}", answer.notes);
    }
}

/// A count that reads nothing beyond its filter's column goes through the
/// side index only for a value very few rows hold. One row in twenty
/// scattered over the table is read by decoding the column through, which
/// costs a third of fetching those rows one by one; the same value beside
/// a second column to decode is still worth the index, which spares that
/// column for the other nineteen.
#[test]
fn a_count_by_its_filter_column_alone_asks_the_side_index_for_rare_values_only() {
    let fixture = fixture(150_000, false, true);
    let pending = fixture
        .model
        .values()
        .filter(|entry| entry.state == "pending")
        .count();
    assert!(
        pending * 32 > fixture.model.len() && pending * 4 < fixture.model.len(),
        "{pending} rows are pending: the case needs a share between the two limits"
    );
    let common = run(
        &fixture,
        "SELECT COUNT(*) FROM ledger WHERE state = 'pending'",
    );
    assert_eq!(common.rows, vec![vec![pending.to_string()]]);
    assert_eq!(common.index_slices, 0, "{}", common.notes);

    let beside = run(
        &fixture,
        "SELECT COUNT(*), SUM(qty) FROM ledger WHERE state = 'pending'",
    );
    assert_eq!(beside.rows[0][0], pending.to_string());
    assert!(beside.index_slices > 0, "{}", beside.notes);

    let rare = run(
        &fixture,
        "SELECT COUNT(*) FROM ledger WHERE mark = 'flagged'",
    );
    assert_eq!(rare.rows, vec![vec!["60".to_owned()]]);
    assert!(rare.index_slices > 0, "{}", rare.notes);
}

/// Writes the rows as tab-separated text and prints each answer as
/// `PAIR<tab>layout<tab>sql<tab>rows`, rows sorted, cells joined by `,` and
/// rows by `|`, for a comparison against a live server holding the rows.
#[test]
#[ignore = "dumps rows and answers for a comparison against a live server"]
fn dump_for_a_live_pair() {
    use std::fmt::Write as _;
    let Some(prefix) = std::env::var_os("LOW_CARD_DUMP") else {
        return;
    };
    let prefix = prefix.to_string_lossy().into_owned();
    for writes in [false, true] {
        let layout = if writes { "writes" } else { "settled" };
        let fixture = fixture(120_000, writes, true);
        let mut rows = String::new();
        for (id, entry) in &fixture.model {
            writeln!(
                rows,
                "{id}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                entry.state,
                entry.kind,
                entry.country,
                entry.plan,
                entry.tag,
                entry.mark.as_deref().unwrap_or("\\N"),
                entry.qty
            )
            .expect("row");
        }
        std::fs::write(format!("{prefix}.{layout}"), rows).expect("dump");
        let mut statements = Vec::new();
        for (filter, _) in filters() {
            statements.push(format!(
                "SELECT COUNT(*), SUM(qty), MIN(id), MAX(id) FROM ledger WHERE {filter}"
            ));
            statements.push(format!(
                "SELECT state, COUNT(*), SUM(qty) FROM ledger WHERE {filter} GROUP BY state"
            ));
        }
        for sql in [
            "SELECT state, COUNT(*), SUM(qty), SUM(kind = 'refund') FROM ledger GROUP BY state",
            "SELECT state, kind, COUNT(*) FROM ledger GROUP BY state, kind",
            "SELECT kind, country, COUNT(*), SUM(qty) FROM ledger GROUP BY kind, country",
            "SELECT DISTINCT state FROM ledger",
            "SELECT DISTINCT mark FROM ledger",
            "SELECT COUNT(DISTINCT state), COUNT(DISTINCT plan), COUNT(DISTINCT tag) FROM ledger",
            "SELECT MIN(state), MAX(state), MIN(mark), MAX(kind) FROM ledger",
            "SELECT CASE state WHEN 'active' THEN 'a' WHEN 'closed' THEN 'c' ELSE 'o' END, \
             COUNT(*) FROM ledger GROUP BY 1",
            "SELECT id, state, mark FROM ledger WHERE mark = 'flagged' ORDER BY state, id LIMIT 30",
            "SELECT id, state FROM ledger ORDER BY state, id LIMIT 20",
            "SELECT id, kind FROM ledger ORDER BY kind DESC, id LIMIT 20",
            "SELECT a.state, COUNT(*) FROM ledger a JOIN ledger b ON b.id = a.id + 1 \
             AND b.state = a.state WHERE a.id < 20000 GROUP BY a.state",
        ] {
            statements.push(sql.to_owned());
        }
        for sql in statements {
            let mut answer = run(&fixture, &sql)
                .rows
                .iter()
                .map(|row| row.join(","))
                .collect::<Vec<_>>();
            answer.sort();
            println!("PAIR\t{layout}\t{sql}\t{}", answer.join("|"));
        }
    }
}

mod bench {
    use super::{fixture, run};

    #[test]
    #[ignore = "bench: prints the cost of filters and groups over low-cardinality text"]
    fn filters_and_groups() {
        let rows: u64 = std::env::var("BENCH_ROWS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4_000_000);
        let runs: usize = std::env::var("BENCH_RUNS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(9);
        let fixture = fixture(rows, false, false);
        println!("query\tmedian_ms\tmin_ms\tanswer\tscan");
        let chosen = std::env::var("BENCH_SQL").unwrap_or_default();
        let chosen = chosen
            .split(';')
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
            .collect::<Vec<_>>();
        for sql in [
            "SELECT COUNT(*) FROM ledger WHERE state = 'closed'",
            "SELECT COUNT(*) FROM ledger WHERE state = 'active'",
            "SELECT COUNT(*) FROM ledger WHERE state IN ('closed', 'pending')",
            "SELECT COUNT(*) FROM ledger WHERE state <> 'active'",
            "SELECT COUNT(*) FROM ledger WHERE state LIKE 'pend%'",
            "SELECT COUNT(*) FROM ledger WHERE mark = 'flagged'",
            "SELECT COUNT(*), SUM(qty) FROM ledger WHERE mark = 'flagged'",
            "SELECT COUNT(*) FROM ledger WHERE mark <> 'normal'",
            "SELECT COUNT(*), SUM(qty) FROM ledger WHERE mark LIKE 'flag%'",
            "SELECT COUNT(*) FROM ledger WHERE mark IS NULL",
            "SELECT COUNT(*) FROM ledger WHERE kind = 'void'",
            "SELECT COUNT(*) FROM ledger WHERE country = 'C39'",
            "SELECT COUNT(*) FROM ledger WHERE plan = 'plan-150'",
            "SELECT COUNT(*) FROM ledger WHERE tag = 'tag-0042'",
            "SELECT COUNT(*), SUM(qty) FROM ledger WHERE state = 'closed'",
            "SELECT state, COUNT(*) FROM ledger GROUP BY state",
            "SELECT state, kind, COUNT(*), SUM(qty) FROM ledger GROUP BY state, kind",
            "SELECT country, COUNT(*), SUM(qty) FROM ledger GROUP BY country",
            "SELECT plan, COUNT(*) FROM ledger GROUP BY plan",
            "SELECT tag, COUNT(*) FROM ledger GROUP BY tag",
            "SELECT COUNT(DISTINCT kind), COUNT(DISTINCT plan) FROM ledger",
            "SELECT SUM(state = 'closed'), SUM(kind = 'refund') FROM ledger",
            "SELECT kind, SUM(state = 'closed'), COUNT(CASE mark WHEN 'normal' THEN 1 END) \
             FROM ledger GROUP BY kind",
            "SELECT id, state FROM ledger ORDER BY state, id LIMIT 10",
        ]
        .into_iter()
        .filter(|_| chosen.is_empty())
        .chain(chosen.iter().copied())
        {
            let _ = run(&fixture, sql);
            let mut times = Vec::new();
            let mut last = None;
            for _ in 0..runs {
                let started = std::time::Instant::now();
                last = Some(run(&fixture, sql));
                times.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            times.sort_by(f64::total_cmp);
            let answer = last.expect("a run");
            let shown = answer
                .rows
                .first()
                .map(|row| row.join(","))
                .unwrap_or_default();
            println!(
                "{sql}\t{:.1}\t{:.1}\t{} rows, first {shown}\tskipped_blocks={} {}",
                times[times.len() / 2],
                times[0],
                answer.rows.len(),
                answer.skipped,
                answer.notes
            );
        }
    }
}
