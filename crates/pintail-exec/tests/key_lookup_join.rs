//! `ORDER BY` one table's key with a `LIMIT`, over a join on the other
//! table's primary key, reads a few rows of each table rather than all of
//! both, and answers exactly what the full join and sort answer.

use std::time::{Duration, Instant};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableSnapshot, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const EVENTS: i64 = 130_000;
const USERS: i64 = 100_000;

fn database() -> DatabaseId {
    DatabaseId::new(1)
}

fn events_id() -> TableId {
    TableId::new(1)
}

fn users_id() -> TableId {
    TableId::new(2)
}

fn schema(id: u32, owner: bool) -> TableSchema {
    let mut columns = vec![
        Column::new(1, "id", DataType::Int64, false),
        Column::new(2, "name", DataType::Utf8, false),
    ];
    if owner {
        columns.push(Column::new(3, "user_id", DataType::Int64, false));
    }
    TableSchema::new(id, columns).expect("schema")
}

/// Every eleventh event names a user that does not exist.
fn owner(id: i64) -> i64 {
    if id % 11 == 0 {
        USERS + id
    } else {
        (id * 7_919) % USERS + 1
    }
}

fn event(id: i64, name: String, user_id: i64, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
        vec![Value::Int64(id), Value::Utf8(name), Value::Int64(user_id)],
        version,
        false,
    )
}

fn user(id: i64, name: String, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
        vec![Value::Int64(id), Value::Utf8(name)],
        version,
        deleted,
    )
}

fn event_name(id: i64) -> String {
    if id % 3 == 0 {
        "shared".to_owned()
    } else {
        format!("event-{id}")
    }
}

fn user_name(id: i64) -> String {
    if id % 4 == 0 {
        "shared".to_owned()
    } else {
        format!("user-{id}")
    }
}

struct Answer {
    rows: Vec<String>,
    plan: String,
    users_blocks: usize,
    events_blocks: usize,
    elapsed: Duration,
}

struct Fixture {
    events: TableSnapshot,
    users: TableSnapshot,
    catalog: CatalogSnapshot,
    _stores: (TableStore, TableStore),
    _directories: (tempfile::TempDir, tempfile::TempDir),
}

impl Fixture {
    fn new() -> Self {
        let options = || StoreOptions {
            background_compaction: false,
            ..StoreOptions::default()
        };
        let events_directory = tempfile::tempdir().expect("events directory");
        let users_directory = tempfile::tempdir().expect("users directory");
        let mut events =
            TableStore::open(events_directory.path(), schema(1, true), options()).expect("events");
        let mut users =
            TableStore::open(users_directory.path(), schema(2, false), options()).expect("users");
        events
            .bulk_ingest_snapshot(
                (1..=EVENTS)
                    .map(|id| event(id, event_name(id), owner(id), 1))
                    .collect(),
            )
            .expect("events snapshot");
        users
            .bulk_ingest_snapshot(
                (1..=USERS)
                    .map(|id| user(id, user_name(id), 1, false))
                    .collect(),
            )
            .expect("users snapshot");
        // Replicated writes still in the memtable: an update, a delete, and
        // a user the eleventh events point past the snapshot at.
        users
            .ingest(vec![
                user(2, "renamed-2".to_owned(), 2, false),
                user(4, user_name(4), 2, true),
                user(USERS + 22, "late".to_owned(), 2, false),
            ])
            .expect("user writes");
        events
            .ingest(vec![event(6, "event-6b".to_owned(), 4, 2)])
            .expect("event writes");
        let entries = [
            TableEntry::new(
                events_id(),
                "events",
                schema(1, true),
                TableStatistics::with_row_count(EVENTS.cast_unsigned()),
            )
            .expect("events entry")
            .with_key_columns([1])
            .expect("events key"),
            TableEntry::new(
                users_id(),
                "users",
                schema(2, false),
                TableStatistics::with_row_count(USERS.cast_unsigned()),
            )
            .expect("users entry")
            .with_key_columns([1])
            .expect("users key"),
        ];
        let catalog =
            CatalogSnapshot::new([DatabaseEntry::new(database(), "app", entries).expect("app")])
                .expect("catalog");
        Self {
            events: events.snapshot(),
            users: users.snapshot(),
            catalog,
            _stores: (events, users),
            _directories: (events_directory, users_directory),
        }
    }

    fn run(&self, sql: &str) -> Answer {
        let provider = SnapshotScanProvider::new([
            (database(), events_id(), &self.events),
            (database(), users_id(), &self.users),
        ])
        .expect("provider");
        let started = Instant::now();
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("physical plan");
        let plan = format!("{physical:?}");
        let mut execution =
            Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
                .unwrap_or_else(|error| panic!("start {sql}: {error}"));
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("pull {sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                let values = batch
                    .columns()
                    .iter()
                    .map(|column| column.value(row).expect("value"))
                    .collect::<Vec<_>>();
                rows.push(format!("{values:?}"));
            }
        }
        let elapsed = started.elapsed();
        Answer {
            rows,
            plan,
            users_blocks: provider
                .scan_stats(database(), users_id())
                .unwrap_or_default()
                .blocks_read,
            events_blocks: provider
                .scan_stats(database(), events_id())
                .unwrap_or_default()
                .blocks_read,
            elapsed,
        }
    }
}

/// Each query ordered by `e.id`, which the join can yield in order, and by
/// `-e.id DESC`, the same order through an expression it cannot, so the
/// second answer comes from the full join and sort.
const CASES: [(&str, bool); 10] = [
    (
        "SELECT e.id, e.name, u.name FROM events AS e INNER JOIN users AS u ON e.id = u.id \
         WHERE e.id >= 3 ORDER BY {key} LIMIT 5",
        true,
    ),
    (
        "SELECT e.id, e.name, u.name FROM events AS e LEFT JOIN users AS u ON e.id = u.id \
         WHERE e.id >= 3 ORDER BY {key} LIMIT 5",
        true,
    ),
    (
        "SELECT e.id, u.id, u.name FROM events AS e JOIN users AS u ON e.user_id = u.id \
         ORDER BY {key} LIMIT 7",
        true,
    ),
    (
        "SELECT e.id, u.name FROM events AS e LEFT JOIN users AS u \
         ON e.user_id = u.id AND u.id > 100 ORDER BY {key} LIMIT 9 OFFSET 4",
        true,
    ),
    (
        "SELECT e.id, e.name, u.name FROM events AS e JOIN users AS u \
         ON e.user_id = u.id AND u.name <> e.name ORDER BY {key} LIMIT 6",
        true,
    ),
    (
        "SELECT u.name, e.id FROM users AS u JOIN events AS e ON e.id = u.id \
         ORDER BY {key} LIMIT 5",
        true,
    ),
    (
        "SELECT e.name FROM events AS e JOIN users AS u ON e.id = u.id ORDER BY {key} LIMIT 3",
        true,
    ),
    (
        "SELECT e.id, u.name FROM events AS e LEFT JOIN users AS u ON e.user_id = u.id \
         WHERE e.id BETWEEN 20 AND 40 ORDER BY {key} LIMIT 30",
        true,
    ),
    (
        "SELECT e.id, u.name FROM events AS e JOIN users AS u ON e.user_id = u.id \
         ORDER BY {key} LIMIT 60000",
        true,
    ),
    (
        "SELECT e.id FROM events AS e LEFT JOIN users AS u ON e.user_id = u.id \
         WHERE u.id IS NULL ORDER BY {key} LIMIT 5",
        false,
    ),
];

#[test]
fn a_limit_in_the_driving_key_order_answers_as_the_full_sort_does() {
    let fixture = Fixture::new();
    for (template, rewritten) in CASES {
        let fast = fixture.run(&template.replace("{key}", "e.id"));
        let reference = fixture.run(&template.replace("{key}", "-e.id DESC"));
        eprintln!(
            "{:>9.3?} against {:>9.3?}, users blocks {} against {}: {template}",
            fast.elapsed, reference.elapsed, fast.users_blocks, reference.users_blocks
        );
        assert!(!reference.plan.contains("KeyLookupJoin"), "{template}");
        assert_eq!(
            fast.plan.contains("KeyLookupJoin"),
            rewritten,
            "{template}: {}",
            fast.plan
        );
        assert!(!fast.rows.is_empty(), "{template}");
        assert_eq!(fast.rows, reference.rows, "{template}");
        // Never much slower than the full join and sort it replaces.
        assert!(
            fast.elapsed < reference.elapsed * 2 + Duration::from_millis(20),
            "{template}: {:?} against {:?}",
            fast.elapsed,
            reference.elapsed
        );
    }
}

/// A join whose driving side is pinned to a few of its keys, written with
/// the key itself and with `e.id + 0`, which pins nothing, so the second
/// answer comes from the hash join building all of users.
const PINNED: [(&str, bool); 6] = [
    (
        "SELECT e.id, u.name FROM events AS e JOIN users AS u ON u.id = e.user_id \
         WHERE {key} = 5000",
        true,
    ),
    (
        "SELECT e.id, u.name FROM events AS e LEFT JOIN users AS u ON u.id = e.user_id \
         WHERE {key} IN (6, 11, 5000, 77, 129999)",
        true,
    ),
    (
        "SELECT e.id, e.name, u.name FROM events AS e JOIN users AS u \
         ON u.id = e.user_id AND u.name <> e.name WHERE {key} IN (2, 3, 4, 8) AND e.name <> 'x'",
        true,
    ),
    (
        "SELECT e.id, u.name FROM events AS e LEFT JOIN users AS u \
         ON u.id = e.user_id AND u.id > 100 WHERE {key} IN (6, 22, 44, 50000)",
        true,
    ),
    (
        "SELECT e.id, u.name FROM events AS e JOIN users AS u ON u.id = e.user_id \
         WHERE {key} = 999999",
        true,
    ),
    (
        "SELECT e.id, u.name FROM events AS e JOIN users AS u ON u.id = e.user_id \
         WHERE {key} NOT IN (1, 2)",
        false,
    ),
];

#[test]
fn a_join_driven_by_a_few_pinned_keys_answers_as_the_hash_join_does() {
    let fixture = Fixture::new();
    for (template, rewritten) in PINNED {
        let fast = fixture.run(&template.replace("{key}", "e.id"));
        let reference = fixture.run(&template.replace("{key}", "e.id + 0"));
        eprintln!(
            "{:>9.3?} against {:>9.3?}, users blocks {} against {}: {template}",
            fast.elapsed, reference.elapsed, fast.users_blocks, reference.users_blocks
        );
        assert!(!reference.plan.contains("KeyLookupJoin"), "{template}");
        assert_eq!(
            fast.plan.contains("KeyLookupJoin"),
            rewritten,
            "{template}: {}",
            fast.plan
        );
        let (mut fast_rows, mut reference_rows) = (fast.rows, reference.rows);
        fast_rows.sort();
        reference_rows.sort();
        assert_eq!(fast_rows, reference_rows, "{template}");
        // Scattered keys may cost as many reads as the whole of users, but
        // never more.
        assert!(
            fast.users_blocks <= reference.users_blocks,
            "{template}: the lookup read {} blocks of users against {}",
            fast.users_blocks,
            reference.users_blocks
        );
    }
    // One pinned key reads the one block of users its row names.
    let (template, _) = PINNED[0];
    let fast = fixture.run(&template.replace("{key}", "e.id"));
    let reference = fixture.run(&template.replace("{key}", "e.id + 0"));
    assert!(
        fast.users_blocks * 4 <= reference.users_blocks,
        "{} blocks of users against {}",
        fast.users_blocks,
        reference.users_blocks
    );
}

/// `key IN (constants)` reads the runs of keys it lists, not the span
/// between the least and the greatest, and keeps the scan's key order.
#[test]
fn a_scan_pinned_to_scattered_keys_reads_only_their_runs() {
    let fixture = Fixture::new();
    for template in [
        "SELECT id, name FROM events WHERE {key} IN (129000, 3, 64000, 4, 5, NULL, 129999, 7)",
        "SELECT id, name FROM events WHERE {key} IN (1, 130000) ORDER BY id",
        "SELECT id FROM events WHERE {key} IN (6, 6, 6)",
        "SELECT id FROM events WHERE {key} IN (-5, 999999)",
        "SELECT COUNT(*), MIN(name) FROM events WHERE {key} IN (100, 100000, 50000)",
    ] {
        let fast = fixture.run(&template.replace("{key}", "id"));
        let reference = fixture.run(&template.replace("{key}", "id + 0"));
        eprintln!(
            "{:>9.3?} against {:>9.3?}, events blocks {} against {}: {template}",
            fast.elapsed, reference.elapsed, fast.events_blocks, reference.events_blocks
        );
        assert_eq!(fast.rows, reference.rows, "{template}");
        assert!(
            fast.events_blocks * 2 <= reference.events_blocks,
            "{template}: {} blocks against {}",
            fast.events_blocks,
            reference.events_blocks
        );
    }
}

#[test]
fn a_small_limit_reads_a_few_rows_of_the_looked_up_table() {
    let fixture = Fixture::new();
    let template = "SELECT e.id, e.name, u.name FROM events AS e INNER JOIN users AS u \
                    ON e.id = u.id WHERE e.id >= 3 ORDER BY {key} LIMIT 5";
    // Warm both paths once so neither pays first-touch costs.
    fixture.run(&template.replace("{key}", "e.id"));
    fixture.run(&template.replace("{key}", "-e.id DESC"));
    let fast = fixture.run(&template.replace("{key}", "e.id"));
    let reference = fixture.run(&template.replace("{key}", "-e.id DESC"));
    eprintln!(
        "{:?} against {:?}; users blocks {} against {}",
        fast.elapsed, reference.elapsed, fast.users_blocks, reference.users_blocks
    );
    assert!(
        fast.users_blocks * 4 <= reference.users_blocks,
        "the lookup read {} blocks of users against {} for the full join",
        fast.users_blocks,
        reference.users_blocks
    );
    assert!(
        fast.elapsed * 4 < reference.elapsed,
        "{:?} against {:?}",
        fast.elapsed,
        reference.elapsed
    );
}
