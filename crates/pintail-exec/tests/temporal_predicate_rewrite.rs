//! `DATE(c)`, `CAST(c AS DATE)` and `YEAR(c)` predicates must answer exactly
//! what the unrewritten expression answers, over NULLs and day boundaries,
//! and the rewritten form must actually prune: that is the whole point of
//! the rewrite (a 10M-row `DATE(created_at) BETWEEN` scan measured 15 s
//! against 4 ms for the plain range it now becomes).
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, PhysicalScanStats, SnapshotScanProvider,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{
    Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value, format_date_days,
    parse_date_days,
};

/// Rows per segment; each bulk ingest publishes its own segment, so a
/// selective day range has whole segments to skip.
const BATCH: u64 = 2_000;
const BATCHES: u64 = 8;

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    /// Chronological rows from 2026-07-01, 96 rows per day, with every
    /// seventh datetime and every fifth date NULL, and one midnight row per
    /// day so the boundary between `d 23:59:59` and `d+1 00:00:00` is real.
    fn new() -> Self {
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "created_at", DataType::DateTime64 { fsp: 0 }, true),
                Column::new(3, "day", DataType::Date32, true),
                Column::new(4, "amount", DataType::Int64, false),
            ],
        )
        .expect("schema");
        let directory = tempfile::tempdir().expect("directory");
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("table");
        let start = parse_date_days("2026-07-01").expect("start");
        for batch in 0..BATCHES {
            let rows = (batch * BATCH..(batch + 1) * BATCH)
                .map(|id| {
                    let day = start + i64::try_from(id / 96).expect("small");
                    let second = (id % 96) * 900; // 0 .. 23:45:00 in 15-minute steps
                    let date = format_date_days(day).expect("date");
                    let created_at = if id % 7 == 0 {
                        Value::Null
                    } else {
                        Value::Utf8(format!(
                            "{date} {:02}:{:02}:{:02}",
                            second / 3600,
                            (second % 3600) / 60,
                            second % 60
                        ))
                    };
                    let day_value = if id % 5 == 0 {
                        Value::Null
                    } else {
                        Value::Utf8(date)
                    };
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            created_at,
                            day_value,
                            Value::Int64(i64::try_from(id % 13).expect("small")),
                        ],
                        id + 1,
                        false,
                    )
                })
                .collect();
            table.bulk_ingest_snapshot(rows).expect("ingest");
        }
        let entry = TableEntry::new(
            TableId::new(1),
            "events",
            schema,
            TableStatistics::with_row_count(BATCH * BATCHES),
        )
        .expect("entry");
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database");
        Self {
            _directory: directory,
            table,
            catalog: CatalogSnapshot::new([database]).expect("catalog"),
        }
    }

    fn run(&self, sql: &str, optimize: bool) -> (Vec<String>, PhysicalScanStats) {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let statement = parse_statement(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&statement)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let logical = LogicalPlanner::plan(bound);
        let logical = if optimize {
            Optimizer::optimize(logical)
        } else {
            logical
        };
        let physical = PhysicalPlanner::plan(logical, Collation::default()).expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
                .expect("execution");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
        {
            for row in batch.selection().selected_rows() {
                let values: Vec<_> = batch
                    .columns()
                    .iter()
                    .map(|column| column.value(row).expect("value"))
                    .collect();
                rows.push(format!("{values:?}"));
            }
        }
        rows.sort();
        (
            rows,
            provider
                .scan_stats(DatabaseId::new(1), TableId::new(1))
                .unwrap_or_default(),
        )
    }

    /// The optimized answer, asserted equal to the unoptimized one.
    fn agree(&self, predicate: &str) -> (Vec<String>, PhysicalScanStats) {
        let sql = format!("SELECT id, created_at, day FROM events WHERE {predicate}");
        let (reference, _) = self.run(&sql, false);
        let (optimized, stats) = self.run(&sql, true);
        assert_eq!(optimized, reference, "{predicate}");
        (optimized, stats)
    }
}

#[test]
fn rewritten_date_predicates_answer_exactly_what_the_function_answers() {
    let fixture = Fixture::new();
    for column in ["created_at", "day"] {
        for wrapped in [format!("DATE({column})"), format!("CAST({column} AS DATE)")] {
            for predicate in [
                format!("{wrapped} = '2026-07-03'"),
                format!("{wrapped} <> '2026-07-03'"),
                format!("{wrapped} < '2026-07-03'"),
                format!("{wrapped} <= '2026-07-03'"),
                format!("{wrapped} > '2026-07-03'"),
                format!("{wrapped} >= '2026-07-03'"),
                format!("'2026-07-03' < {wrapped}"),
                format!("{wrapped} BETWEEN '2026-07-02' AND '2026-07-04'"),
                format!("{wrapped} NOT BETWEEN '2026-07-02' AND '2026-07-04'"),
                format!("{wrapped} IN ('2026-07-01', '2026-07-05', '2030-01-01')"),
                format!("{wrapped} NOT IN ('2026-07-01', '2026-07-05')"),
                format!("NOT ({wrapped} = '2026-07-03')"),
                format!("{wrapped} = '2026-07-03' OR amount = 12"),
                format!("{wrapped} BETWEEN '2026-07-02' AND '2026-07-04' AND amount > 6"),
                // Left alone by the rewrite, so this pins the runtime answer
                // the rewrite must keep matching.
                format!("{wrapped} = '2026-07-03 10:00:00'"),
                format!("{wrapped} = '2026-02-30'"),
                format!("{wrapped} IS NULL"),
            ] {
                let (rows, _) = fixture.agree(&predicate);
                // The untouched shapes (time-part and impossible literals)
                // may select nothing; every rewritten shape must hit rows.
                assert!(
                    predicate.contains(":00") || predicate.contains("02-30") || !rows.is_empty(),
                    "{predicate} should select rows in this fixture"
                );
            }
        }
        for predicate in [
            format!("YEAR({column}) = 2026"),
            format!("YEAR({column}) <> 2026"),
            format!("YEAR({column}) < 2026"),
            format!("YEAR({column}) >= 2026"),
            format!("YEAR({column}) BETWEEN 2025 AND 2026"),
            format!("YEAR({column}) IN (2024, 2026)"),
            format!("YEAR({column}) = 2025"),
        ] {
            fixture.agree(&predicate);
        }
    }
}

#[test]
fn date_predicates_select_the_expected_rows_across_a_midnight_boundary() {
    let fixture = Fixture::new();
    // 2026-07-03 is ids 192..288, minus the NULL datetimes (every seventh).
    let (rows, _) = fixture.agree("DATE(created_at) = '2026-07-03'");
    assert_eq!(rows.len(), (192..288).filter(|id| id % 7 != 0).count());
    // The inclusive upper day keeps its midnight row and nothing after it.
    let (rows, _) = fixture.agree("DATE(created_at) BETWEEN '2026-07-02' AND '2026-07-03'");
    assert_eq!(rows.len(), (96..288).filter(|id| id % 7 != 0).count());
    assert!(rows.iter().any(|row| row.contains("2026-07-03 00:00:00")));
    assert!(!rows.iter().any(|row| row.contains("2026-07-04 00:00:00")));
    // NULL datetimes never satisfy an inequality either.
    let (rows, _) = fixture.agree("DATE(created_at) <> '2026-07-03'");
    assert_eq!(
        rows.len(),
        (0..BATCH * BATCHES)
            .filter(|id| id % 7 != 0 && !(192..288).contains(id))
            .count()
    );
    // A midnight literal names its day, as MySQL promotes the date before
    // comparing. The unrewritten runtime compares the rendered date text
    // against the longer literal and misses, so this is asserted directly.
    let (day_rows, _) = fixture.agree("DATE(created_at) = '2026-07-03'");
    let (midnight_rows, _) = fixture.run(
        "SELECT id, created_at, day FROM events WHERE DATE(created_at) = '2026-07-03 00:00:00'",
        true,
    );
    assert_eq!(midnight_rows, day_rows);
    // Year bounds on a DATE column.
    let (rows, _) = fixture.agree("YEAR(day) = 2026");
    assert_eq!(
        rows.len(),
        (0..BATCH * BATCHES).filter(|id| id % 5 != 0).count()
    );
}

#[test]
fn rewritten_predicates_prune_segments_and_blocks() {
    let fixture = Fixture::new();
    for predicate in [
        "DATE(created_at) BETWEEN '2026-07-02' AND '2026-07-03'",
        "DATE(created_at) = '2026-07-10'",
        "CAST(created_at AS DATE) < '2026-07-02'",
        "DATE(day) BETWEEN '2026-07-02' AND '2026-07-03'",
        "YEAR(created_at) = 2025",
    ] {
        let (_, stats) = fixture.agree(predicate);
        assert!(
            stats.segments_pruned > 0,
            "{predicate} must prune whole segments: {stats:?}"
        );
    }
    let (_, stats) = fixture.agree("DATE(created_at) = '2026-07-10 10:00:00'");
    assert_eq!(
        stats.segments_pruned, 0,
        "a literal with a time part is not rewritten and cannot prune: {stats:?}"
    );
}
