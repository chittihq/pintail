//! The settled memo keeps answering a table whose last changes are still in
//! its memtable, and a limited sort over a memoized aggregate answers from
//! the rows it kept.
//!
//! The memo used to serve only a table with an empty memtable. A replicated
//! table holds the rows of its last change in the memtable until a flush,
//! so after any update or delete the memo stopped answering for that table
//! until the next flush - for a quiet table, indefinitely. The memtable is
//! now part of the entry's identity: the first run after a change executes,
//! the runs after it are hits, and the next change gives the memtable a new
//! identity. Every answer here is checked against a fresh execution over a
//! table holding the same rows in another directory, which no entry names.
//!
//! The limited sort used to order every replayed group on every hit. Its
//! own answer is now kept under the aggregate's key, so a hit serves the
//! kept rows - ties in the order the sort first gave them - and nothing is
//! ordered again.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    ExecCounters, Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    take_exec_counters,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableSnapshot, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Rows flushed to segments before each test changes anything.
const SETTLED: u64 = 4_000;
/// Groups of `grp`: every group holds the same number of rows, so a sort
/// by the count ties across all of them.
const GROUPS: u64 = 500;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "grp", DataType::Int64, false),
            Column::new(3, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64, amount: i64, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(i64::try_from(id % GROUPS).expect("small")),
            Value::Int64(amount),
        ],
        version,
        false,
    )
}

fn deleted(id: u64, version: u64) -> StoredRow {
    let live = row(id, 0, version);
    StoredRow::new(live.key().clone(), live.values().to_vec(), version, true)
}

fn settled_amount(id: u64) -> i64 {
    i64::try_from(id % 7).expect("small")
}

fn catalog() -> CatalogSnapshot {
    let entry = TableEntry::new(
        TableId::new(1),
        "ledger",
        schema(),
        TableStatistics::with_row_count(SETTLED),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    CatalogSnapshot::new([DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("db")])
        .expect("catalog")
}

/// A table of the settled rows, flushed, in a directory of its own.
fn settled_table() -> (tempfile::TempDir, TableStore) {
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
    table
        .bulk_ingest_snapshot(
            (0..SETTLED)
                .map(|id| row(id, settled_amount(id), id + 1))
                .collect(),
        )
        .expect("rows");
    table.flush().expect("flush");
    (directory, table)
}

/// Every row `sql` answers over `snapshot`, with this thread's counters.
fn run(snapshot: &TableSnapshot, sql: &str) -> (Vec<Vec<Value>>, ExecCounters) {
    let catalog = catalog();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), snapshot)])
        .expect("provider");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let _ = take_exec_counters();
    let mut execution =
        Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
            .expect("execution");
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            rows.push(
                (0..batch.columns().len())
                    .map(|column| {
                        batch
                            .column(column)
                            .and_then(|column| column.value_owned(row))
                            .unwrap_or(Value::Null)
                    })
                    .collect(),
            );
        }
    }
    (rows, take_exec_counters())
}

/// `sql` over a new table in another directory holding exactly `rows`,
/// flushed: an execution no memo entry can answer.
fn fresh(rows: Vec<StoredRow>, sql: &str) -> Vec<Vec<Value>> {
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
    table.bulk_ingest_snapshot(rows).expect("rows");
    table.flush().expect("flush");
    let (answer, counters) = run(&table.snapshot(), sql);
    assert_eq!(counters.settled_memo_hits, 0, "a fresh table is executed");
    answer
}

/// The live rows after `changes` were applied over the settled ones.
fn live_rows(changes: &[StoredRow]) -> Vec<StoredRow> {
    let mut rows = (0..SETTLED)
        .map(|id| (id, row(id, settled_amount(id), id + 1)))
        .collect::<std::collections::BTreeMap<_, _>>();
    for change in changes {
        let id = match change.key().parts() {
            [KeyPart::UInt64(id)] => *id,
            other => panic!("unexpected key {other:?}"),
        };
        if change.is_deleted() {
            rows.remove(&id);
        } else {
            rows.insert(id, change.clone());
        }
    }
    rows.into_values()
        .enumerate()
        .map(|(position, row)| {
            StoredRow::new(
                row.key().clone(),
                row.values().to_vec(),
                u64::try_from(position).expect("small") + 1,
                false,
            )
        })
        .collect()
}

/// AVG cannot be merged onto a finished answer, so only the memtable's own
/// entry can serve it after a change.
const BY_GROUP: &str = "SELECT grp % 10 AS g, COUNT(*) AS c, AVG(amount) AS a, SUM(amount) AS s \
                        FROM ledger GROUP BY grp % 10 ORDER BY g";

#[test]
fn updates_and_deletes_in_the_memtable_are_served_from_the_memo_after_one_run() {
    let (_directory, mut table) = settled_table();
    let (settled, _) = run(&table.snapshot(), BY_GROUP);
    assert_eq!(
        run(&table.snapshot(), BY_GROUP).1.settled_memo_hits,
        1,
        "the settled answer is a hit"
    );
    assert_eq!(settled, fresh(live_rows(&[]), BY_GROUP));

    // An update of settled rows and a delete: neither is an insert above
    // the segments, so no delta merge can answer.
    let mut changes = (100..120)
        .map(|id| row(id, 1_000, SETTLED + id))
        .collect::<Vec<_>>();
    changes.extend((200..210).map(|id| deleted(id, SETTLED + id)));
    table.ingest(changes.clone()).expect("change");

    let (first, counters) = run(&table.snapshot(), BY_GROUP);
    assert_eq!(counters.settled_memo_hits, 0, "the first run executes");
    assert_eq!(counters.settled_delta_merges, 0);
    let expected = fresh(live_rows(&changes), BY_GROUP);
    assert_eq!(first, expected);
    assert_ne!(first, settled, "the change moved the answer");

    for _ in 0..3 {
        let (again, counters) = run(&table.snapshot(), BY_GROUP);
        assert_eq!(
            counters.settled_memo_hits, 1,
            "the memtable's entry answers"
        );
        assert_eq!(again, expected, "a hit equals a fresh execution");
    }

    // The next change names the memtable anew: one execution, then hits.
    let next = vec![row(5, 50_000, 2 * SETTLED)];
    table.ingest(next.clone()).expect("next change");
    changes.extend(next);
    let expected = fresh(live_rows(&changes), BY_GROUP);
    let (after, counters) = run(&table.snapshot(), BY_GROUP);
    assert_eq!(
        counters.settled_memo_hits, 0,
        "the old memtable's entry is not served"
    );
    assert_eq!(after, expected);
    let (again, counters) = run(&table.snapshot(), BY_GROUP);
    assert_eq!(counters.settled_memo_hits, 1);
    assert_eq!(again, expected);

    // A flush moves the rows to a segment: a new generation with an empty
    // memtable, which the memo takes up again.
    table.flush().expect("flush");
    let (flushed, counters) = run(&table.snapshot(), BY_GROUP);
    assert_eq!(
        counters.settled_memo_hits, 0,
        "a new generation executes once"
    );
    assert_eq!(flushed, expected);
    let (again, counters) = run(&table.snapshot(), BY_GROUP);
    assert_eq!(counters.settled_memo_hits, 1, "and is a hit after it");
    assert_eq!(again, expected);
}

#[test]
fn an_insert_only_delta_merges_once_and_is_a_hit_after_it() {
    let (_directory, mut table) = settled_table();
    let sql = "SELECT COUNT(*), SUM(amount), MAX(amount) FROM ledger";
    let _ = run(&table.snapshot(), sql);

    let inserted = (SETTLED..SETTLED + 40)
        .map(|id| row(id, 9, id + 1))
        .collect::<Vec<_>>();
    table.ingest(inserted.clone()).expect("insert");
    let expected = fresh(live_rows(&inserted), sql);

    let (merged, counters) = run(&table.snapshot(), sql);
    assert_eq!(
        counters.settled_delta_merges, 1,
        "the delta extends the base"
    );
    assert_eq!(merged, expected);
    let (again, counters) = run(&table.snapshot(), sql);
    assert_eq!(counters.settled_delta_merges, 0, "the merge was kept");
    assert_eq!(counters.settled_memo_hits, 1);
    assert_eq!(again, expected);
}

/// A replica reopens its reader after every replicated change. A reopen
/// used to be a new store instance, so the settled entry from before the
/// change could never be found and the delta merge never ran. While the
/// process holds the table's lock, reopens of one lineage share it.
#[test]
fn a_reopened_reader_finds_the_entry_its_predecessor_kept() {
    let (directory, mut table) = settled_table();
    let sql = "SELECT COUNT(*), SUM(amount) FROM ledger";
    let first = TableSnapshot::open(directory.path(), schema()).expect("reader");
    let _ = run(&first, sql);
    let reopened = TableSnapshot::open(directory.path(), schema()).expect("reader");
    assert_eq!(reopened.instance(), first.instance());
    assert_eq!(
        run(&reopened, sql).1.settled_memo_hits,
        1,
        "an unchanged reopen is a hit"
    );

    let inserted = (SETTLED..SETTLED + 8)
        .map(|id| row(id, 3, id + 1))
        .collect::<Vec<_>>();
    table.ingest(inserted.clone()).expect("insert");
    let changed = TableSnapshot::open(directory.path(), schema()).expect("reader");
    let (merged, counters) = run(&changed, sql);
    assert_eq!(counters.settled_delta_merges, 1, "the base entry was found");
    assert_eq!(merged, fresh(live_rows(&inserted), sql));

    // Two opens replay the WAL each for itself; their memtables are never
    // taken for one another.
    let again = TableSnapshot::open(directory.path(), schema()).expect("reader");
    assert_ne!(again.memtable_identity(), changed.memtable_identity());

    // A directory replaced outside its writer ends the lineage.
    pintail_store::publish_changes_under(directory.path());
    let replaced = TableSnapshot::open(directory.path(), schema()).expect("reader");
    assert_ne!(replaced.instance(), first.instance());
}

/// Every group holds the same count, so `ORDER BY c DESC LIMIT k` ties
/// across all of them and the rows kept are decided by arrival order.
const TIED: &str = "SELECT grp, COUNT(*) AS c FROM ledger GROUP BY grp ORDER BY c DESC LIMIT 10";
const TIED_ALL: &str = "SELECT grp, COUNT(*) AS c FROM ledger GROUP BY grp ORDER BY c DESC";
/// Ties on the total broken by the group, as a report of top spenders is.
const SPENDERS: &str = "SELECT grp, SUM(amount) AS s FROM ledger GROUP BY grp \
                        ORDER BY s DESC, grp LIMIT 10";

#[test]
fn a_limited_sort_over_a_memoized_aggregate_is_served_without_sorting() {
    let (_directory, mut table) = settled_table();
    for (limited, unlimited) in [(TIED, TIED_ALL), (SPENDERS, "")] {
        let (first, counters) = run(&table.snapshot(), limited);
        assert_eq!(counters.settled_top_k_hits, 0, "the first run sorts");
        assert_eq!(first.len(), 10);
        if !unlimited.is_empty() {
            // The kept rows are the full sort's first ten, ties included.
            let (all, _) = run(&table.snapshot(), unlimited);
            assert_eq!(first, all[..10].to_vec());
        }
        assert_eq!(first, fresh(live_rows(&[]), limited));
        for _ in 0..2 {
            let (again, counters) = run(&table.snapshot(), limited);
            assert_eq!(counters.settled_top_k_hits, 1, "the kept rows answer");
            assert_eq!(counters.rows_sorted, 0, "nothing is ordered again");
            assert_eq!(again, first, "the same rows, ties in the same order");
        }
    }

    // A change invalidates the kept rows with the aggregate beneath them.
    let changes = vec![row(7, 10_000, 2 * SETTLED)];
    table.ingest(changes.clone()).expect("change");
    let expected = fresh(live_rows(&changes), SPENDERS);
    let (after, counters) = run(&table.snapshot(), SPENDERS);
    assert_eq!(
        counters.settled_top_k_hits, 0,
        "the change is sorted afresh"
    );
    assert_eq!(after, expected);
    assert_eq!(after[0][0], Value::Int64(7), "the changed group leads");
    let (again, counters) = run(&table.snapshot(), SPENDERS);
    assert_eq!(counters.settled_top_k_hits, 1);
    assert_eq!(again, expected);
}
