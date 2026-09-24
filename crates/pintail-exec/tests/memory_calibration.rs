//! What the memory tracker charges against what an operator actually holds.
//!
//! Every operator reserves an estimate of its bytes, and the budget, spill
//! and admission decisions all read those estimates. An estimate far below
//! the truth lets a query outgrow its ceiling unseen; one far above spills
//! and refuses work that would have fit. This binary runs one query per
//! operator shape and samples, at every pulled batch, the tracker's charge
//! and the bytes the allocator has actually handed out since the query
//! started, then reports each shape's peak of both.
//!
//! It is its own test binary, not a module of `integration`, because it
//! installs the allocator the server ships with as the global one - the
//! allocator's own counter is the measurement.
//!
//! The measurement is `#[ignore]`d:
//! `cargo test --release -p pintail-exec --test memory_calibration --
//! --ignored --nocapture`.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
use tikv_jemalloc_ctl::{epoch, stats};

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const ROWS: u64 = 1_000_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "grp", DataType::UInt64, false),
            Column::new(3, "name", DataType::Utf8, false),
            Column::new(
                4,
                "amount",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                false,
            ),
        ],
    )
    .expect("schema")
}

/// Bytes the allocator has handed out and not taken back, process-wide.
fn allocated() -> usize {
    epoch::advance().expect("advance the allocator's statistics epoch");
    stats::allocated::read().expect("read allocated bytes")
}

/// One million orders over 100,000 groups and 50,000 distinct names.
fn fixture() -> (tempfile::TempDir, TableStore, CatalogSnapshot) {
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
    table
        .bulk_ingest_snapshot(
            (0..ROWS)
                .map(|id| {
                    let group = id.wrapping_mul(2_654_435_761) % 100_000;
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            Value::UInt64(group),
                            Value::Utf8(format!("customer-name-{:05}", group % 50_000)),
                            Value::Utf8(format!("{}.{:02}", (id * 7_919) % 5_000, id % 100)),
                        ],
                        id + 1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("rows");
    let entry = TableEntry::new(
        TableId::new(1),
        "orders",
        schema(),
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    (directory, table, catalog)
}

#[test]
#[ignore = "measurement"]
fn tracker_charges_against_allocated_bytes() {
    let (_directory, table, catalog) = fixture();
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");

    let shapes = [
        (
            "integer key, SUM/COUNT",
            "SELECT grp, COUNT(*), SUM(amount) FROM orders GROUP BY grp",
        ),
        (
            "text key, COUNT",
            "SELECT name, COUNT(*) FROM orders GROUP BY name",
        ),
        (
            "expression key (general path)",
            "SELECT name, LENGTH(name) AS n, COUNT(*) FROM orders GROUP BY name, n",
        ),
        ("COUNT(DISTINCT)", "SELECT COUNT(DISTINCT grp) FROM orders"),
        ("full sort", "SELECT id, name FROM orders ORDER BY name, id"),
        (
            "top-k",
            "SELECT id, name FROM orders ORDER BY name DESC, id LIMIT 100",
        ),
        (
            "hash join",
            "SELECT COUNT(*) FROM orders o JOIN orders p ON p.id = o.grp",
        ),
        (
            "window",
            "SELECT id, ROW_NUMBER() OVER (PARTITION BY grp ORDER BY id) FROM orders",
        ),
        ("DISTINCT rows", "SELECT DISTINCT grp, name FROM orders"),
    ];
    eprintln!(
        "{:<32} {:>12} {:>12} {:>9}",
        "shape", "charged MiB", "actual MiB", "charged/actual"
    );
    for (label, sql) in shapes {
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let baseline = allocated();
        // The allocator side is sampled on its own thread: a blocking
        // operator builds its whole state inside the first pull, and its
        // peak - a hash table mid-resize - is there, not at a batch boundary.
        let running = std::sync::atomic::AtomicBool::new(true);
        let peak = std::sync::atomic::AtomicUsize::new(0);
        let mut charged = 0_usize;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while running.load(std::sync::atomic::Ordering::Relaxed) {
                    peak.fetch_max(
                        allocated().saturating_sub(baseline),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
            let mut execution =
                Execution::start(physical, &provider, 1 << 34, Collation::default())
                    .expect("start");
            loop {
                charged = charged.max(execution.memory().peak());
                // Each batch is dropped as soon as it is pulled, so what stays
                // allocated is the operators' own state.
                if execution
                    .next_batch()
                    .unwrap_or_else(|error| panic!("pull {sql}: {error}"))
                    .is_none()
                {
                    break;
                }
            }
            drop(execution);
            running.store(false, std::sync::atomic::Ordering::Relaxed);
        });
        let actual = peak.load(std::sync::atomic::Ordering::Relaxed);
        #[allow(clippy::cast_precision_loss)]
        let mib = |bytes: usize| bytes as f64 / f64::from(1_u32 << 20);
        #[allow(clippy::cast_precision_loss)]
        let ratio = charged as f64 / actual.max(1) as f64;
        eprintln!(
            "{label:<32} {:>12.1} {:>12.1} {ratio:>9.2}",
            mib(charged),
            mib(actual)
        );
    }
}
