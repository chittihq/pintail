//! What a query reserves against what its process grows by.
//!
//! The tracker's peak is the sum of what the operators said they held. The
//! process's resident set is what they did hold. The two cannot be compared
//! inside one test process: the fixture's own allocations, the other tests
//! and whatever the allocator kept from them are all in the resident set.
//! So this runs one query per fresh process, over tables another process
//! wrote:
//!
//! ```text
//! bin=$(ls -t target/recovery/deps/integration-* | grep -v '\.d$' | head -1)
//! export PINTAIL_PROBE_DIR=$HOME/probe
//! $bin --ignored --exact memory_probe::write_the_tables
//! for shape in $($bin --ignored --exact memory_probe::run_one_shape --nocapture \
//!         | sed -n 's/^SHAPE //p'); do
//!     PINTAIL_PROBE_SHAPE=$shape $bin --ignored --exact \
//!         memory_probe::run_one_shape --nocapture | grep '^PROBE'
//! done
//! ```
//!
//! Each `PROBE` line carries the shape, the tracker's peak, the resident set
//! before the query and its high-water mark after, all in bytes. Every shape
//! has a `scan` twin reading the same columns into an ungrouped fold, which
//! reserves next to nothing: the growth of the twin is the scan's decoded
//! columns and the process-wide caches, and what the shape grows beyond its
//! twin is what its operator state really cost. That difference over the
//! tracked peak is the ratio to watch; well above one, a reservation
//! under-counts.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const EVENTS: u64 = 2_000_000;
const ACCOUNTS: u64 = 200_000;
const LEDGER: u64 = 400_000;

fn events_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "account", DataType::Int64, false),
            Column::new(3, "tag", DataType::Utf8, false),
            Column::new(4, "owner", DataType::Utf8, false),
            Column::new(5, "stamp", DataType::DateTime64 { fsp: 0 }, false),
            Column::new(6, "amount", DataType::Int64, false),
            Column::new(7, "score", DataType::Int64, false),
            Column::new(8, "member", DataType::Int64, false),
            Column::new(9, "note", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn accounts_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "code", DataType::Utf8, false),
            Column::new(3, "tier", DataType::Int64, false),
            Column::new(4, "region", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn ledger_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "amount", DataType::Int64, false),
            Column::new(3, "memo", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn signed(value: u64) -> i64 {
    i64::try_from(value).expect("fits")
}

/// A value spread over the signed range: nothing dense to index by.
fn spread(id: u64) -> i64 {
    id.wrapping_mul(0x9E37_79B9_7F4A_7C15).cast_signed() >> 2
}

fn stored(id: u64, version: u64, values: Vec<Value>) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        values,
        version,
        false,
    )
}

fn event(id: u64) -> StoredRow {
    let account = (id * 7) % ACCOUNTS;
    let minute = id % 400_000;
    stored(
        id,
        id + 1,
        vec![
            Value::UInt64(id),
            Value::Int64(signed(account)),
            Value::Utf8(format!("tag-{:02}", id % 25)),
            Value::Utf8(format!("acct-{account:06}")),
            Value::Utf8(format!(
                "2026-{:02}-{:02} {:02}:{:02}:00",
                1 + minute / 43_200 % 12,
                1 + minute / 1_440 % 28,
                minute / 60 % 24,
                minute % 60
            )),
            Value::Int64(signed(id % 1_000) - 500),
            Value::Int64(signed((id * 31) % 20_000)),
            Value::Int64(spread(id / 2)),
            Value::Utf8(format!("note-{:04}", id % 977)),
        ],
    )
}

fn directory() -> std::path::PathBuf {
    std::env::var_os("PINTAIL_PROBE_DIR")
        .map(std::path::PathBuf::from)
        .expect("PINTAIL_PROBE_DIR names the tables' directory")
}

#[test]
#[ignore = "writes the probe's tables; see the module docs"]
fn write_the_tables() {
    let directory = directory();
    std::fs::create_dir_all(&directory).expect("directory");
    let open = |name: &str, schema: TableSchema| {
        TableStore::open(directory.join(name), schema, StoreOptions::default()).expect("table")
    };
    let mut events = open("events", events_schema());
    events
        .bulk_ingest_snapshot((0..EVENTS).map(event).collect())
        .expect("events");
    let mut accounts = open("accounts", accounts_schema());
    accounts
        .bulk_ingest_snapshot(
            (0..ACCOUNTS)
                .map(|id| {
                    stored(
                        id,
                        id + 1,
                        vec![
                            Value::UInt64(id),
                            Value::Utf8(format!("acct-{id:06}")),
                            Value::Int64(signed(id / 3)),
                            Value::Utf8(format!("region-{:02}", id % 12)),
                        ],
                    )
                })
                .collect(),
        )
        .expect("accounts");
    // A table written after its snapshot: flushed changes lie over the
    // snapshot's rows, so a scan resolves the two layers.
    let ledger_row = |id: u64, version: u64, amount: u64| {
        stored(
            id,
            version,
            vec![
                Value::UInt64(id),
                Value::Int64(signed(amount)),
                Value::Utf8(format!("memo-{:05}", id % 40_000)),
            ],
        )
    };
    let mut ledger = open("ledger", ledger_schema());
    ledger
        .bulk_ingest_snapshot(
            (0..LEDGER)
                .map(|id| ledger_row(id, id + 1, id % 900))
                .collect(),
        )
        .expect("ledger");
    for round in 0..4_u64 {
        ledger
            .ingest(
                (0..LEDGER)
                    .filter(|id| id % 4 == round)
                    .map(|id| ledger_row(id, LEDGER + 1 + round, id % 700 + round))
                    .collect(),
            )
            .expect("changes");
        ledger.flush().expect("flush");
    }
}

/// Each operator shape and, under `scan:`, the ungrouped fold reading the
/// same columns.
const SHAPES: &[(&str, &str)] = &[
    (
        "fused-join-fold",
        "SELECT a.tier, COUNT(*), SUM(e.amount), MIN(e.amount), MAX(e.score), SUM(e.score), \
         AVG(e.amount) FROM events e JOIN accounts a ON a.id = e.account GROUP BY a.tier",
    ),
    (
        "scan:fused-join-fold",
        "SELECT COUNT(account), SUM(amount), MAX(score) FROM events",
    ),
    (
        "small-group-fold",
        "SELECT tag, COUNT(*), MIN(note), MAX(stamp), SUM(amount) FROM events GROUP BY tag",
    ),
    (
        "scan:small-group-fold",
        "SELECT COUNT(tag), MIN(note), MAX(stamp), SUM(amount) FROM events",
    ),
    (
        "two-pass-dense-slots",
        "SELECT tag, COUNT(*), SUM(amount), COUNT(DISTINCT member) FROM events GROUP BY tag",
    ),
    (
        "scan:two-pass-dense-slots",
        "SELECT COUNT(tag), SUM(amount), SUM(member) FROM events",
    ),
    (
        "distinct-unit-key-sets",
        "SELECT tag, COUNT(DISTINCT stamp), COUNT(DISTINCT owner) FROM events GROUP BY tag",
    ),
    (
        "scan:distinct-unit-key-sets",
        "SELECT COUNT(tag), MAX(stamp), MAX(owner) FROM events",
    ),
    (
        "range-fold",
        "SELECT account, COUNT(*), SUM(amount), MAX(score) FROM events GROUP BY account",
    ),
    (
        "scan:range-fold",
        "SELECT COUNT(account), SUM(amount), MAX(score) FROM events",
    ),
    (
        "two-pass-maps",
        "SELECT member, COUNT(*), SUM(amount) FROM events GROUP BY member",
    ),
    (
        "scan:two-pass-maps",
        "SELECT COUNT(member), SUM(amount) FROM events",
    ),
    (
        "composite-hash",
        "SELECT account, tag, COUNT(*), SUM(amount) FROM events GROUP BY account, tag",
    ),
    (
        "scan:composite-hash",
        "SELECT COUNT(account), COUNT(tag), SUM(amount) FROM events",
    ),
    (
        "hash-join-build",
        "SELECT COUNT(*), MAX(a.region), SUM(e.amount) FROM events e JOIN accounts a \
         ON a.code = e.owner",
    ),
    (
        "scan:hash-join-build",
        "SELECT COUNT(owner), SUM(amount) FROM events",
    ),
    // No twin: the query reserves nothing, and all of its growth is the
    // resolved layers the process keeps between scans.
    (
        "resolved-layer-cache",
        "SELECT COUNT(*), SUM(amount), MAX(memo) FROM ledger",
    ),
];

/// A field of `/proc/self/status` in bytes; `None` where there is none.
fn status_bytes(field: &str) -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with(field))?;
    let kilobytes = line.split_whitespace().nth(1)?.parse::<usize>().ok()?;
    Some(kilobytes * 1024)
}

#[test]
#[ignore = "one query per process; see the module docs"]
fn run_one_shape() {
    let Some(shape) = std::env::var_os("PINTAIL_PROBE_SHAPE") else {
        for (name, _) in SHAPES {
            println!("SHAPE {name}");
        }
        return;
    };
    let shape = shape.to_string_lossy().into_owned();
    let sql = SHAPES
        .iter()
        .find(|(name, _)| *name == shape)
        .map_or_else(|| panic!("no shape named {shape}"), |(_, sql)| *sql);
    let directory = directory();
    let specs = [
        ("events", events_schema(), EVENTS),
        ("accounts", accounts_schema(), ACCOUNTS),
        ("ledger", ledger_schema(), LEDGER),
    ];
    let tables = specs
        .iter()
        .map(|(name, schema, _)| {
            TableStore::open(
                directory.join(name),
                schema.clone(),
                StoreOptions::default(),
            )
            .expect("table")
        })
        .collect::<Vec<_>>();
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        DatabaseId::new(1),
        "app",
        specs.iter().zip(1_u64..).map(|((name, schema, rows), id)| {
            TableEntry::new(
                TableId::new(id),
                *name,
                schema.clone(),
                TableStatistics::with_row_count(*rows),
            )
            .expect("entry")
            .with_key_columns([1])
            .expect("key")
        }),
    )
    .expect("database")])
    .expect("catalog");
    let snapshots = tables.iter().map(TableStore::snapshot).collect::<Vec<_>>();
    let provider = SnapshotScanProvider::new(
        snapshots
            .iter()
            .zip(1_u64..)
            .map(|(snapshot, id)| (DatabaseId::new(1), TableId::new(id), snapshot)),
    )
    .expect("provider");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    // The high-water mark starts over from here: opening the tables is
    // not the query's.
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    let before = status_bytes("VmRSS:").unwrap_or(0);
    let mut execution =
        Execution::start_profiled(physical, &provider, 1 << 34, None, Collation::default())
            .expect("start");
    let mut rows = 0_usize;
    while let Some(batch) = execution.next_batch().expect("batch") {
        rows += batch.visible_row_count();
    }
    let high = status_bytes("VmHWM:").unwrap_or(0);
    let notes = execution
        .profile()
        .map(|profile| {
            profile
                .operators
                .iter()
                .filter_map(|node| node.note.as_deref())
                .filter(|note| !note.starts_with("decompressed"))
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default();
    println!(
        "PROBE\t{shape}\t{}\t{before}\t{high}\t{rows}\t{notes}",
        execution.memory().peak()
    );
}
