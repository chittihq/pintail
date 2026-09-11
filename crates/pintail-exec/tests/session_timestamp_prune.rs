//! A session in a zone other than UTC reads every source `TIMESTAMP`
//! through a conversion, and a filter written against that reading used to
//! hide the column from storage: the whole table was read where a UTC
//! session skipped all but a segment. A fixed offset shifts every stored
//! value by the same amount, so the filter is rewritten onto the column
//! with its literal shifted the other way - the same rows, and the same
//! pruning the UTC session gets. A named zone is not one shift, so it is
//! left as it was written.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, PhysicalScanStats, SnapshotScanProvider,
    set_session_time_zone,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Rows per segment; each bulk ingest publishes its own, so a selective
/// range has whole segments to skip.
const BATCH: u64 = 2_000;
const BATCHES: u64 = 8;

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    /// One row every fifteen minutes from 2026-07-01 00:00:00 UTC, in the
    /// `seen` column, which the source declares `TIMESTAMP`.
    fn new() -> Self {
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "seen", DataType::DateTime64 { fsp: 0 }, true).with_timestamp(true),
            ],
        )
        .expect("schema");
        let directory = tempfile::tempdir().expect("directory");
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("table");
        for batch in 0..BATCHES {
            let rows = (batch * BATCH..(batch + 1) * BATCH)
                .map(|id| {
                    let minutes = id * 15;
                    let day = 1 + minutes / (60 * 24);
                    let hour = (minutes / 60) % 24;
                    let minute = minutes % 60;
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            Value::Utf8(format!("2026-07-{day:02} {hour:02}:{minute:02}:00")),
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

    /// `sql` under `zone`, as its rows and what the scan read.
    fn run(&self, sql: &str, zone: Option<&str>) -> (Vec<String>, PhysicalScanStats) {
        assert!(set_session_time_zone(zone), "{zone:?}");
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
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
            Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
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
        assert!(set_session_time_zone(None), "restore the host zone");
        (rows, stats)
    }
}

#[test]
fn a_filter_in_a_fixed_offset_session_answers_the_same_rows_and_prunes() {
    let fixture = Fixture::new();
    // 2026-07-03 05:30:00 +05:30 is 2026-07-03 00:00:00 UTC, so the two
    // sessions name the same instants and must select the same rows - the
    // shifted `seen` values aside, which is why the query reads `id` alone.
    let local = "SELECT id FROM events \
                 WHERE seen >= '2026-07-03 05:30:00' AND seen < '2026-07-04 05:30:00'";
    let utc = "SELECT id FROM events \
               WHERE seen >= '2026-07-03 00:00:00' AND seen < '2026-07-04 00:00:00'";
    let (offset_rows, offset_stats) = fixture.run(local, Some("+05:30"));
    let (utc_rows, utc_stats) = fixture.run(utc, None);
    assert_eq!(
        offset_rows, utc_rows,
        "the same instants select the same rows"
    );
    assert!(!offset_rows.is_empty());
    assert_eq!(
        (offset_stats.blocks_pruned, offset_stats.blocks_read),
        (utc_stats.blocks_pruned, utc_stats.blocks_read),
        "an offset session reads what a UTC one reads: {offset_stats:?} against {utc_stats:?}"
    );
    assert!(
        offset_stats.blocks_pruned > offset_stats.blocks_read,
        "a one-day range skips most of the table: {offset_stats:?}"
    );
}

#[test]
fn a_filter_in_a_named_zone_session_is_left_as_written() {
    let fixture = Fixture::new();
    // Asia/Kolkata is +05:30 all year, but the rewrite takes only offsets:
    // a named zone can hold two offsets, and one shift cannot answer for
    // both. The answer still has to be right.
    let (named_rows, _) = fixture.run(
        "SELECT id FROM events \
         WHERE seen >= '2026-07-03 05:30:00' AND seen < '2026-07-04 05:30:00'",
        Some("Asia/Kolkata"),
    );
    let (offset_rows, _) = fixture.run(
        "SELECT id FROM events \
         WHERE seen >= '2026-07-03 05:30:00' AND seen < '2026-07-04 05:30:00'",
        Some("+05:30"),
    );
    assert_eq!(named_rows, offset_rows);
}
