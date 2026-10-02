//! A scan under a limit reads what the limit can take, not the table.
//!
//! The planner tells a scan how many rows the limit above it wants; these
//! tests pin that storage reads about that many - asserted by the scan's
//! own counters, never by time - and that the rows are the ones a full
//! read would have produced first, through every way storage serves a
//! scan: one settled segment, interleaved segments merged row by row, and
//! a memtable that updates, deletes and inserts over a segment.
//!
//! Each answer is compared with a model of the table, so a scan that
//! stopped before a deleted row was replaced, or counted a row its filter
//! then dropped, fails here.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, PhysicalScanStats, SnapshotScanProvider,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const COLUMNS: u64 = 5;

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
    /// `id -> (grp, amount)` of every visible row.
    model: BTreeMap<i64, (i64, i64)>,
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::Int64, false),
            Column::new(2, "grp", DataType::Int64, true),
            Column::new(3, "amount", DataType::Int64, true),
            Column::new(4, "note", DataType::Utf8, true),
            Column::new(5, "tag", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

fn row(id: i64, grp: i64, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
        vec![
            Value::Int64(id),
            Value::Int64(grp),
            Value::Int64(id * 3),
            Value::Utf8(format!("note {id}")),
            Value::Utf8(format!("t{}", id.rem_euclid(11))),
        ],
        version,
        deleted,
    )
}

fn values(id: i64, grp: i64) -> Vec<Value> {
    row(id, grp, 1, false).values().to_vec()
}

/// `grp` of the row first written for `id`: a few common values, and one
/// only the table's last rows carry.
fn first_grp(id: i64, rows: i64) -> i64 {
    if id >= rows - 40 {
        77
    } else {
        id.rem_euclid(7)
    }
}

impl Fixture {
    /// `rows` keys from zero, written as `segments` flushed segments whose
    /// key ranges interleave when there are several.
    fn new(rows: i64, segments: i64) -> Self {
        let schema = schema();
        let directory = tempfile::tempdir().expect("directory");
        let mut table = TableStore::open(
            directory.path(),
            schema.clone(),
            StoreOptions {
                background_compaction: false,
                ..StoreOptions::default()
            },
        )
        .expect("table");
        let mut model = BTreeMap::new();
        for segment in 0..segments {
            let batch = (0..rows)
                .filter(|id| id.rem_euclid(segments) == segment)
                .map(|id| {
                    let grp = first_grp(id, rows);
                    model.insert(id, (grp, id * 3));
                    row(id, grp, 1, false)
                })
                .collect();
            table.ingest(batch).expect("segment");
            table.flush().expect("flush");
        }
        let entry = TableEntry::new(
            TableId::new(1),
            "t",
            schema,
            TableStatistics::with_row_count(u64::try_from(rows).expect("rows")),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        let catalog =
            CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
            ])
            .expect("catalog");
        Self {
            _directory: directory,
            table,
            catalog,
            model,
        }
    }

    /// Deletes the table's first `deleted` keys, rewrites every `step`th
    /// key after them and inserts a few past the end, all in the memtable.
    fn write_through_memtable(&mut self, deleted: i64, step: i64) {
        let last = *self.model.keys().next_back().expect("rows");
        let mut writes = Vec::new();
        for id in 0..deleted {
            writes.push(row(id, 0, 2, true));
            self.model.remove(&id);
        }
        for id in (deleted..last).step_by(usize::try_from(step).expect("step")) {
            writes.push(row(id, 500, 2, false));
            self.model.insert(id, (500, id * 3));
        }
        for id in [last + 5, last + 9] {
            writes.push(row(id, 501, 2, false));
            self.model.insert(id, (501, id * 3));
        }
        self.table.ingest(writes).expect("memtable writes");
    }

    fn run(&self, sql: &str) -> (Vec<Vec<Value>>, PhysicalScanStats) {
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let mut execution =
            Execution::start(plan, &provider, 1 << 30, Collation::default()).expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value(row).cloned().unwrap_or(Value::Null))
                        .collect::<Vec<_>>(),
                );
            }
        }
        drop(execution);
        let stats = provider
            .scan_stats(DatabaseId::new(1), TableId::new(1))
            .unwrap_or_default();
        (rows, stats)
    }

    /// The model's rows that `keep` passes, in key order, after `offset`
    /// of them, at most `count`.
    fn expected(
        &self,
        keep: impl Fn(i64, i64) -> bool,
        offset: usize,
        count: usize,
    ) -> Vec<Vec<Value>> {
        self.model
            .iter()
            .filter(|(id, (grp, _))| keep(**id, *grp))
            .skip(offset)
            .take(count)
            .map(|(id, (grp, _))| values(*id, *grp))
            .collect()
    }
}

#[test]
fn a_limit_reads_the_rows_it_returns_from_a_settled_segment() {
    let fixture = Fixture::new(100_000, 1);
    let (rows, stats) = fixture.run("SELECT * FROM t LIMIT 10");
    assert_eq!(rows, fixture.expected(|_, _| true, 0, 10));
    assert_eq!(stats.values_decoded, 10 * COLUMNS, "{stats:?}");
    assert!(stats.blocks_decoded <= 5, "{stats:?}");

    // An offset is rows the limit takes and drops: they are read too.
    let (rows, stats) = fixture.run("SELECT * FROM t LIMIT 10 OFFSET 25");
    assert_eq!(rows, fixture.expected(|_, _| true, 25, 10));
    assert_eq!(stats.values_decoded, 35 * COLUMNS, "{stats:?}");

    // The table's key order is the scan's own: no sort, the same read.
    let (rows, stats) = fixture.run("SELECT * FROM t ORDER BY id LIMIT 10");
    assert_eq!(rows, fixture.expected(|_, _| true, 0, 10));
    assert_eq!(stats.values_decoded, 10 * COLUMNS, "{stats:?}");

    // A projection narrows the read as well as the limit does.
    let (rows, stats) = fixture.run("SELECT grp FROM t LIMIT 3");
    assert_eq!(rows.len(), 3);
    assert_eq!(stats.values_decoded, 3, "{stats:?}");

    // A limit past the table's end reads the table.
    let (rows, _) = fixture.run("SELECT * FROM t LIMIT 200000");
    assert_eq!(rows.len(), 100_000);
}

#[test]
fn a_filtered_limit_stops_once_enough_rows_passed() {
    let fixture = Fixture::new(100_000, 1);
    let everything = 100_000 * COLUMNS;

    // One row in seven passes: a few thousand values, not the table.
    let (rows, stats) = fixture.run("SELECT * FROM t WHERE grp = 3 LIMIT 10");
    assert_eq!(rows, fixture.expected(|_, grp| grp == 3, 0, 10));
    assert!(stats.values_decoded < 20_000, "{stats:?}");

    let (rows, stats) = fixture.run("SELECT * FROM t WHERE grp = 3 LIMIT 10 OFFSET 15");
    assert_eq!(rows, fixture.expected(|_, grp| grp == 3, 15, 10));
    assert!(stats.values_decoded < 20_000, "{stats:?}");

    // A key range starts the read where the range starts.
    let (rows, stats) = fixture.run("SELECT * FROM t WHERE id > 60000 ORDER BY id LIMIT 10");
    assert_eq!(rows, fixture.expected(|id, _| id > 60_000, 0, 10));
    assert!(stats.values_decoded < 20_000, "{stats:?}");

    let (rows, stats) =
        fixture.run("SELECT * FROM t WHERE id > 60000 AND grp = 5 ORDER BY id LIMIT 4");
    assert_eq!(
        rows,
        fixture.expected(|id, grp| id > 60_000 && grp == 5, 0, 4)
    );
    assert!(stats.values_decoded < 20_000, "{stats:?}");

    // Rows only the end of the table holds: the scan reads on until it
    // has them, in fetches that grow, and returns the same rows.
    let (rows, stats) = fixture.run("SELECT * FROM t WHERE grp = 77 LIMIT 5");
    assert_eq!(rows, fixture.expected(|_, grp| grp == 77, 0, 5));
    assert!(stats.values_decoded <= everything, "{stats:?}");

    // More rows wanted than pass: every passing row, once.
    let (rows, _) = fixture.run("SELECT * FROM t WHERE grp = 77 LIMIT 500");
    assert_eq!(rows, fixture.expected(|_, grp| grp == 77, 0, 500));
    assert_eq!(rows.len(), 40);

    // A predicate storage cannot judge by itself still answers exactly.
    let (rows, _) = fixture.run("SELECT * FROM t WHERE note LIKE '%999' LIMIT 7 OFFSET 3");
    assert_eq!(
        rows,
        fixture.expected(|id, _| id.rem_euclid(1000) == 999, 3, 7)
    );
}

#[test]
fn a_limit_reads_past_rows_the_memtable_removed() {
    let mut fixture = Fixture::new(100_000, 1);
    // The first 20,000 keys are gone and every 97th after them rewritten:
    // the segment's first blocks supply no row at all.
    fixture.write_through_memtable(20_000, 97);
    for (limit, offset) in [
        (1, 0),
        (10, 0),
        (10, 30),
        (100, 5),
        (5_000, 0),
        (90_000, 10),
    ] {
        let (rows, _) = fixture.run(&format!("SELECT * FROM t LIMIT {limit} OFFSET {offset}"));
        assert_eq!(
            rows,
            fixture.expected(|_, _| true, offset, limit),
            "limit {limit} offset {offset}"
        );
        let (rows, _) = fixture.run(&format!(
            "SELECT * FROM t ORDER BY id LIMIT {limit} OFFSET {offset}"
        ));
        assert_eq!(
            rows,
            fixture.expected(|_, _| true, offset, limit),
            "ordered limit {limit} offset {offset}"
        );
        let (rows, _) = fixture.run(&format!(
            "SELECT * FROM t WHERE grp = 500 OR grp = 2 LIMIT {limit} OFFSET {offset}"
        ));
        assert_eq!(
            rows,
            fixture.expected(|_, grp| grp == 500 || grp == 2, offset, limit),
            "filtered limit {limit} offset {offset}"
        );
    }
    // The rows written past the segment's end are the table's last.
    let (rows, _) = fixture.run("SELECT * FROM t WHERE grp = 501 LIMIT 3");
    assert_eq!(rows, fixture.expected(|_, grp| grp == 501, 0, 3));
    assert_eq!(rows.len(), 2);

    // Still a bounded read: the blocks holding the deleted keys and the
    // first live ones, not the eighty thousand rows after them.
    let (rows, stats) = fixture.run("SELECT * FROM t LIMIT 10");
    assert_eq!(rows.len(), 10);
    assert!(stats.values_decoded < 60_000 * COLUMNS, "{stats:?}");
}

#[test]
fn a_limit_over_merged_segments_resolves_only_the_rows_it_needs() {
    let mut fixture = Fixture::new(30_000, 3);
    for (limit, offset) in [(1, 0), (10, 0), (10, 45), (2_000, 7), (29_999, 0)] {
        let (rows, _) = fixture.run(&format!("SELECT * FROM t LIMIT {limit} OFFSET {offset}"));
        assert_eq!(
            rows,
            fixture.expected(|_, _| true, offset, limit),
            "limit {limit} offset {offset}"
        );
        let (rows, _) = fixture.run(&format!(
            "SELECT * FROM t WHERE grp = 4 LIMIT {limit} OFFSET {offset}"
        ));
        assert_eq!(
            rows,
            fixture.expected(|_, grp| grp == 4, offset, limit),
            "filtered limit {limit} offset {offset}"
        );
    }
    fixture.write_through_memtable(300, 53);
    for (limit, offset) in [(1, 0), (10, 0), (10, 45), (2_000, 7), (29_999, 0)] {
        let (rows, _) = fixture.run(&format!("SELECT * FROM t LIMIT {limit} OFFSET {offset}"));
        assert_eq!(
            rows,
            fixture.expected(|_, _| true, offset, limit),
            "limit {limit} offset {offset} under writes"
        );
        let (rows, _) = fixture.run(&format!(
            "SELECT * FROM t WHERE grp = 500 OR grp = 4 ORDER BY id LIMIT {limit} OFFSET {offset}"
        ));
        assert_eq!(
            rows,
            fixture.expected(|_, grp| grp == 500 || grp == 4, offset, limit),
            "filtered limit {limit} offset {offset} under writes"
        );
    }
}

impl Fixture {
    /// The model's last rows that `keep` passes, newest key first, after
    /// `offset` of them, at most `count`.
    fn expected_from_end(
        &self,
        keep: impl Fn(i64, i64) -> bool,
        offset: usize,
        count: usize,
    ) -> Vec<Vec<Value>> {
        self.model
            .iter()
            .rev()
            .filter(|(id, (grp, _))| keep(**id, *grp))
            .skip(offset)
            .take(count)
            .map(|(id, (grp, _))| values(*id, *grp))
            .collect()
    }

    fn check_from_end(&self, what: &str) {
        for (limit, offset) in [(1, 0), (10, 0), (10, 37), (3_000, 5), (200_000, 0)] {
            let (rows, _) = self.run(&format!(
                "SELECT * FROM t ORDER BY id DESC LIMIT {limit} OFFSET {offset}"
            ));
            assert_eq!(
                rows,
                self.expected_from_end(|_, _| true, offset, limit),
                "{what}: limit {limit} offset {offset}"
            );
            let (rows, _) = self.run(&format!(
                "SELECT * FROM t WHERE grp = 4 OR grp = 500 OR grp = 77 \
                 ORDER BY id DESC LIMIT {limit} OFFSET {offset}"
            ));
            assert_eq!(
                rows,
                self.expected_from_end(|_, grp| grp == 4 || grp == 500 || grp == 77, offset, limit),
                "{what}: filtered limit {limit} offset {offset}"
            );
        }
    }
}

#[test]
fn the_last_rows_in_key_order_are_read_from_the_end() {
    let fixture = Fixture::new(100_000, 1);
    let (rows, stats) = fixture.run("SELECT * FROM t ORDER BY id DESC LIMIT 10");
    assert_eq!(rows, fixture.expected_from_end(|_, _| true, 0, 10));
    assert_eq!(stats.values_decoded, 10 * COLUMNS, "{stats:?}");

    let (rows, stats) = fixture.run("SELECT * FROM t ORDER BY id DESC LIMIT 10 OFFSET 15");
    assert_eq!(rows, fixture.expected_from_end(|_, _| true, 15, 10));
    assert_eq!(stats.values_decoded, 25 * COLUMNS, "{stats:?}");

    let (rows, stats) =
        fixture.run("SELECT * FROM t WHERE id < 40000 AND grp = 3 ORDER BY id DESC LIMIT 5");
    assert_eq!(
        rows,
        fixture.expected_from_end(|id, grp| id < 40_000 && grp == 3, 0, 5)
    );
    assert!(stats.values_decoded < 20_000, "{stats:?}");

    // Ascending by an expression of the key is no key order: a full read.
    let (rows, stats) = fixture.run("SELECT * FROM t ORDER BY -id LIMIT 10");
    assert_eq!(rows, fixture.expected_from_end(|_, _| true, 0, 10));
    assert_eq!(stats.values_decoded, 100_000 * COLUMNS, "{stats:?}");

    fixture.check_from_end("one segment");
}

#[test]
fn the_last_rows_are_exact_under_writes_and_merges() {
    let mut fixture = Fixture::new(100_000, 1);
    fixture.write_through_memtable(20_000, 97);
    fixture.check_from_end("memtable over a segment");
    // The table's last keys deleted: the end of the segment supplies none.
    let last = *fixture.model.keys().next_back().expect("rows");
    let deletes = (last - 30_000..=last)
        .filter(|id| fixture.model.remove(id).is_some())
        .map(|id| row(id, 0, 3, true))
        .collect();
    fixture.table.ingest(deletes).expect("deletes");
    fixture.check_from_end("the end deleted");

    let mut merged = Fixture::new(30_000, 3);
    merged.check_from_end("interleaved segments");
    merged.write_through_memtable(300, 53);
    merged.check_from_end("interleaved segments under writes");
}
