//! `DATE('<literal>')` names one day for the whole statement, the way
//! `CURDATE()` does, and a range written against it has to reach the scan
//! as a bound. Left unfolded, the interval around it and the comparison
//! above it stayed per-row expressions: every row re-read the literal, and
//! the scan could skip nothing. A literal `DATE` cannot read still warns
//! when the statement runs.

use std::time::Instant;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, PhysicalScanStats, SnapshotScanProvider,
    take_session_conversion_warnings,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    /// `batches` segments of `batch` rows, one every minute from
    /// 2026-01-01 00:00:00 in the DATETIME column `logged`.
    fn new(batch: u64, batches: u64) -> Self {
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "logged", DataType::DateTime64 { fsp: 0 }, true),
            ],
        )
        .expect("schema");
        let directory = tempfile::tempdir().expect("directory");
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("table");
        let start = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");
        for segment in 0..batches {
            let rows = (segment * batch..(segment + 1) * batch)
                .map(|id| {
                    let at = start + chrono::Duration::minutes(i64::try_from(id).expect("id"));
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            Value::Utf8(at.format("%Y-%m-%d %H:%M:%S").to_string()),
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
            "readings",
            schema,
            TableStatistics::with_row_count(batch * batches),
        )
        .expect("entry");
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database");
        Self {
            _directory: directory,
            table,
            catalog: CatalogSnapshot::new([database]).expect("catalog"),
        }
    }

    /// `sql`'s rows, what the scan read, and the warnings it raised.
    fn run(&self, sql: &str) -> (Vec<String>, PhysicalScanStats, u64) {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let _ = take_session_conversion_warnings();
        let statement = parse_statement(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&statement)
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
                .expect("execution");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
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
        let stats = provider
            .scan_stats(DatabaseId::new(1), TableId::new(1))
            .unwrap_or_default();
        let (_, warnings) = take_session_conversion_warnings();
        (rows, stats, warnings)
    }
}

#[test]
fn a_range_from_a_literal_date_prunes_like_the_day_it_names() {
    let fixture = Fixture::new(2_000, 8);
    let (folded, folded_stats, warnings) = fixture.run(
        "SELECT id FROM readings \
         WHERE logged >= DATE_SUB(DATE('2026-01-12 17:45:00'), INTERVAL 2 DAY)",
    );
    let (literal, literal_stats, _) =
        fixture.run("SELECT id FROM readings WHERE logged >= '2026-01-10'");
    assert_eq!(folded, literal, "the same day selects the same rows");
    assert_ne!(folded, [] as [std::string::String; 0]);
    assert_eq!(warnings, 0);
    assert_eq!(
        (folded_stats.segments_pruned, folded_stats.blocks_decoded),
        (literal_stats.segments_pruned, literal_stats.blocks_decoded),
        "DATE('...') reads what the day it names reads: {folded_stats:?} against {literal_stats:?}"
    );
    assert!(
        folded_stats.segments_pruned > folded_stats.segments_read,
        "a range past most of the table skips some of it: {folded_stats:?}"
    );
}

#[test]
fn a_literal_date_cannot_read_still_warns_when_the_statement_runs() {
    let fixture = Fixture::new(100, 1);
    let (rows, _, warnings) =
        fixture.run("SELECT id FROM readings WHERE logged >= DATE('not a day')");
    assert!(rows.is_empty(), "NULL compares false: {rows:?}");
    assert!(warnings > 0, "the unreadable literal warns");
}

/// Measurement: the folded range against the same range spelled as its
/// literal. `cargo test --profile recovery -p pintail-exec --test
/// integration date_literal_fold:: -- --ignored --nocapture`
#[test]
#[ignore = "measurement"]
fn measure_a_literal_date_range() {
    // Two years of minutes; each statement asks for about the last month,
    // with a different day each time so no answer is reused.
    let fixture = Fixture::new(65_536, 16);
    let _ = fixture.run("SELECT SUM(id) FROM readings WHERE logged >= '2027-12-01'");
    let end = chrono::NaiveDate::from_ymd_opt(2027, 12, 30).expect("date");
    for folded in [true, false] {
        let started = Instant::now();
        for days in 30..35 {
            let sql = if folded {
                format!(
                    "SELECT SUM(id) FROM readings \
                     WHERE logged >= DATE_SUB(DATE('{end} 15:00:00'), INTERVAL {days} DAY)"
                )
            } else {
                let day = end - chrono::Duration::days(days);
                format!("SELECT SUM(id) FROM readings WHERE logged >= '{day}'")
            };
            let _ = fixture.run(&sql);
        }
        println!(
            "{:>8.2} ms  {}",
            started.elapsed().as_secs_f64() * 1000.0 / 5.0,
            if folded {
                "DATE_SUB(DATE('...'), INTERVAL n DAY)"
            } else {
                "'YYYY-MM-DD'"
            }
        );
    }
}
