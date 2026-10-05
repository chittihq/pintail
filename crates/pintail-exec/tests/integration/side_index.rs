//! The side index (on unless `PINTAIL_SECONDARY_INDEX=0`) must never
//! change an answer. A filter on a scattered non-key column is answered with
//! the index on and off, from a fresh snapshot, with changes still in the
//! memtable (rows moved into and out of the filtered value, deleted and
//! appended), after a flush has layered a second segment over the first,
//! and after compaction; every answer is checked against a row model.
//!
//! The measurement is `#[ignore]`d, and a repeat answered from the settled
//! memo measures nothing, so it runs with the memo off:
//! `PINTAIL_DISABLE_SETTLED_MEMO=1 cargo test --profile recovery -p pintail-exec
//! --test integration side_index:: -- --ignored --nocapture`.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore, override_side_index};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const OWNER_ID: TableId = TableId::new(1);
const EVENT_ID: TableId = TableId::new(2);
const OWNERS: u64 = 1_009;
const EVENTS: u64 = 200_000;
const KINDS: [&str; 4] = ["open", "held", "closed", "void"];

fn owner_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "region", DataType::Int64, false),
        ],
    )
    .expect("owner schema")
}

fn event_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "account", DataType::Int64, true),
            Column::new(3, "kind", DataType::Utf8, false),
            Column::new(4, "amount", DataType::Int64, false),
            Column::new(5, "note", DataType::Utf8, true),
        ],
    )
    .expect("event schema")
}

/// The event table after a schema change: the wide text column dropped and a
/// nullable integer column added, so the segments written before it read
/// under a schema other than their own.
fn evolved_event_schema() -> TableSchema {
    TableSchema::new(
        2,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "account", DataType::Int64, true),
            Column::new(3, "kind", DataType::Utf8, false),
            Column::new(4, "amount", DataType::Int64, false),
            Column::new(6, "extra", DataType::Int64, true),
        ],
    )
    .expect("evolved event schema")
}

#[derive(Clone, Debug, PartialEq)]
struct Event {
    account: Option<i64>,
    kind: &'static str,
    amount: i64,
}

/// Accounts scatter over the whole table: every block holds most of them,
/// so block extremes skip nothing.
fn initial(id: u64) -> Event {
    Event {
        account: (!id.is_multiple_of(53)).then(|| i64::try_from(id % OWNERS).expect("account")),
        kind: KINDS[usize::try_from(id % 7 % 4).expect("kind")],
        amount: i64::try_from(id % 101).expect("amount") - 40,
    }
}

fn event_row(id: u64, event: &Event, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            event.account.map_or(Value::Null, Value::Int64),
            Value::Utf8(event.kind.to_owned()),
            Value::Int64(event.amount),
            Value::Utf8(format!("event {id} {}", "padding ".repeat(4))),
        ],
        version,
        deleted,
    )
}

struct Fixture {
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    owners: TableStore,
    events: TableStore,
    catalog: CatalogSnapshot,
    model: BTreeMap<u64, Event>,
    version: u64,
}

impl Fixture {
    fn new() -> Self {
        Self::with_memtable_bytes(StoreOptions::default().memtable_bytes)
    }

    fn with_memtable_bytes(memtable_bytes: usize) -> Self {
        let options = StoreOptions {
            background_compaction: false,
            memtable_bytes,
            ..StoreOptions::default()
        };
        let owner_dir = tempfile::tempdir().expect("owner dir");
        let event_dir = tempfile::tempdir().expect("event dir");
        let mut owners = TableStore::open(owner_dir.path(), owner_schema(), options).expect("open");
        owners
            .bulk_ingest_snapshot(
                (0..OWNERS)
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            vec![
                                Value::UInt64(id),
                                Value::Int64(i64::try_from(id % 50).expect("region")),
                            ],
                            1,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("ingest owners");
        let model = (1..=EVENTS)
            .map(|id| (id, initial(id)))
            .collect::<BTreeMap<_, _>>();
        let mut events = TableStore::open(event_dir.path(), event_schema(), options).expect("open");
        events
            .bulk_ingest_snapshot(
                model
                    .iter()
                    .map(|(id, event)| event_row(*id, event, 1, false))
                    .collect(),
            )
            .expect("ingest events");
        Self {
            _dirs: (owner_dir, event_dir),
            owners,
            events,
            catalog: catalog(event_schema()),
            model,
            version: 2,
        }
    }

    /// Publishes the evolved event schema to the store and the catalog.
    fn evolve(&mut self) {
        self.events
            .evolve_schema(evolved_event_schema())
            .expect("evolve");
        self.catalog = catalog(evolved_event_schema());
    }

    /// Moves rows into and out of the probed accounts, deletes some of
    /// theirs, and appends new ones, through the change path.
    fn change(&mut self, round: u64) {
        let mut changes = Vec::new();
        let mut push = |model: &mut BTreeMap<u64, Event>, id: u64, event: Option<Event>| {
            self.version += 1;
            if let Some(event) = event {
                changes.push(event_row(id, &event, self.version, false));
                model.insert(id, event);
            } else {
                let old = model.remove(&id).unwrap_or_else(|| initial(id));
                changes.push(event_row(id, &old, self.version, true));
            }
        };
        let ids = self.model.keys().copied().collect::<Vec<_>>();
        for id in ids {
            let Some(event) = self.model.get(&id).cloned() else {
                continue;
            };
            if event.account == Some(17) && id % 3 == round % 3 {
                // Out of the probed value.
                push(
                    &mut self.model,
                    id,
                    Some(Event {
                        account: Some(18),
                        ..event
                    }),
                );
            } else if event.account == Some(17) && id % 5 == 1 {
                push(&mut self.model, id, None);
            } else if id % 211 == round {
                // Into the probed value, or to NULL.
                push(
                    &mut self.model,
                    id,
                    Some(Event {
                        account: (id % 2 == 0).then_some(17),
                        kind: "open",
                        ..event
                    }),
                );
            }
        }
        for id in EVENTS + round * 1_000..EVENTS + round * 1_000 + 500 {
            push(
                &mut self.model,
                id,
                Some(Event {
                    account: Some(i64::try_from(id % 3).expect("small") * 491 + 17),
                    kind: "held",
                    amount: 7,
                }),
            );
        }
        for batch in changes.chunks(2_000) {
            self.events
                .ingest_cdc(batch.to_vec())
                .expect("change batch");
        }
    }

    fn run(&self, sql: &str, index: bool) -> Vec<Vec<Value>> {
        override_side_index(Some(index));
        let owner_snapshot = self.owners.snapshot();
        let event_snapshot = self.events.snapshot();
        let provider = SnapshotScanProvider::new([
            (DATABASE_ID, OWNER_ID, &owner_snapshot),
            (DATABASE_ID, EVENT_ID, &event_snapshot),
        ])
        .expect("provider");
        let statement = parse_statement(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&statement)
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
        let physical = PhysicalPlanner::plan(logical, Collation::default()).expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 512 * 1024 * 1024, Collation::default())
                .expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for index in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value(index).cloned().expect("value"))
                        .collect::<Vec<_>>(),
                );
            }
        }
        override_side_index(None);
        rows
    }

    /// Every probe, with the index on and off, against the model.
    fn check(&self, stage: &str) {
        let ids = |rows: Vec<Vec<Value>>| {
            rows.iter()
                .map(|row| u64::try_from(int(&row[0])).expect("id"))
                .collect::<Vec<_>>()
        };
        let with = |predicate: &dyn Fn(&Event) -> bool| {
            self.model
                .iter()
                .filter(|(_, event)| predicate(event))
                .map(|(id, _)| *id)
                .collect::<Vec<_>>()
        };
        for index in [false, true] {
            let label = format!("{stage}, index {index}");
            // A different floor per pass keeps the settled aggregate memo
            // from answering the second pass with the first one's result.
            let floor = if index { -1_000 } else { -1_001 };
            let got = ids(self.run(
                "SELECT id, amount FROM events WHERE account = 17 ORDER BY id",
                index,
            ));
            assert_eq!(got, with(&|event| event.account == Some(17)), "{label}");

            let got = ids(self.run(
                "SELECT e.id FROM events e WHERE e.account IN (17, 508, 999) \
                 AND e.kind = 'open' AND e.amount > -30 ORDER BY e.id",
                index,
            ));
            assert_eq!(
                got,
                with(&|event| matches!(event.account, Some(17 | 508 | 999))
                    && event.kind == "open"
                    && event.amount > -30),
                "{label}"
            );

            let got = self.run(
                &format!("SELECT COUNT(*) FROM events WHERE account = 5000 AND amount > {floor}"),
                index,
            );
            assert_eq!(int(&got[0][0]), 0, "{label}");

            // A join's key set: owners of one region against their events.
            let got = self.run(
                &format!(
                    "SELECT COUNT(*), SUM(e.amount) FROM owners o JOIN events e \
                     ON e.account = o.id WHERE o.id IN (17, 18, 508) AND e.amount > {floor}"
                ),
                index,
            );
            let (count, sum) = self
                .model
                .values()
                .filter(|event| matches!(event.account, Some(17 | 18 | 508)))
                .fold((0, 0), |(count, sum), event| {
                    (count + 1, sum + event.amount)
                });
            assert_eq!((int(&got[0][0]), int(&got[0][1])), (count, sum), "{label}");

            let got = self.run(
                &format!(
                    "SELECT o.id, COUNT(e.id) FROM owners o LEFT JOIN events e \
                     ON e.account = o.id AND e.amount > {floor} \
                     WHERE o.id IN (17, 999, 1008) GROUP BY o.id ORDER BY o.id"
                ),
                index,
            );
            let expected = [17_i64, 999, 1_008]
                .iter()
                .map(|owner| {
                    let found = self
                        .model
                        .values()
                        .filter(|event| event.account == Some(*owner))
                        .count();
                    (*owner, i64::try_from(found).expect("count"))
                })
                .collect::<Vec<_>>();
            assert_eq!(
                got.iter()
                    .map(|row| (int(&row[0]), int(&row[1])))
                    .collect::<Vec<_>>(),
                expected,
                "{label}"
            );
        }
        self.check_order_limits(stage);
    }

    /// The first rows in a non-key column's order, which the side index
    /// can narrow the scan to: ascending on a NOT NULL column, descending
    /// on a nullable one (NULLs last), and ascending on the nullable one,
    /// where a NULL sorts first and the index must step aside.
    fn check_order_limits(&self, stage: &str) {
        type Key = fn(&Event) -> Option<i64>;
        let cases: [(&str, usize, Key, bool); 4] = [
            (
                "SELECT id, amount FROM events ORDER BY amount, id LIMIT 25",
                25,
                |event| Some(event.amount),
                false,
            ),
            (
                "SELECT id, kind FROM events ORDER BY amount DESC, id LIMIT 30",
                30,
                |event| Some(event.amount),
                true,
            ),
            (
                "SELECT id FROM events ORDER BY account DESC, id LIMIT 40",
                40,
                |event| event.account,
                true,
            ),
            (
                "SELECT id, account FROM events ORDER BY account, id LIMIT 12",
                12,
                |event| event.account,
                false,
            ),
        ];
        for (sql, limit, key, descending) in cases {
            let mut expected = self
                .model
                .iter()
                .map(|(id, event)| (key(event), *id))
                .collect::<Vec<_>>();
            // NULL first ascending, last descending; ties by id ascending.
            expected.sort_by(|left, right| {
                let order = if descending {
                    right.0.cmp(&left.0)
                } else {
                    left.0.cmp(&right.0)
                };
                order.then(left.1.cmp(&right.1))
            });
            let expected = expected
                .into_iter()
                .take(limit)
                .map(|(_, id)| id)
                .collect::<Vec<_>>();
            for index in [false, true] {
                let got = self
                    .run(sql, index)
                    .iter()
                    .map(|row| u64::try_from(int(&row[0])).expect("id"))
                    .collect::<Vec<_>>();
                assert_eq!(got, expected, "{stage}, index {index}: {sql}");
            }
        }
    }
}

fn catalog(events: TableSchema) -> CatalogSnapshot {
    let entry = |id, name, schema, rows| {
        TableEntry::new(id, name, schema, TableStatistics::with_row_count(rows))
            .expect("entry")
            .with_key_columns([1])
            .expect("key")
    };
    let database = DatabaseEntry::new(
        DATABASE_ID,
        "app",
        [
            entry(OWNER_ID, "owners", owner_schema(), OWNERS),
            entry(EVENT_ID, "events", events, EVENTS),
        ],
    )
    .expect("database");
    CatalogSnapshot::new([database]).expect("catalog")
}

fn int(value: &Value) -> i64 {
    match value {
        Value::Int64(value) => *value,
        Value::UInt64(value) => i64::try_from(*value).expect("fits"),
        Value::Utf8(text) => text.parse().expect("whole decimal"),
        other => panic!("not an integer: {other:?}"),
    }
}

#[test]
fn a_side_index_lookup_answers_exactly_through_every_store_state() {
    let mut fixture = Fixture::new();
    fixture.check("snapshot");
    fixture.change(1);
    fixture.check("changes in the memtable");
    fixture.events.flush().expect("flush");
    fixture.check("flushed over the snapshot");
    fixture.change(2);
    fixture.check("flushed, more changes in the memtable");
    fixture.events.flush().expect("flush");
    fixture.events.compact().expect("compact");
    fixture.check("compacted");
    // The flush and compaction above wrote postings for the probed column;
    // read under the evolved schema they give way to a build from the column.
    fixture.evolve();
    fixture.check("schema changed");
    fixture.events.compact().expect("compact");
    fixture.check("compacted under the changed schema");
}

#[test]
#[ignore = "measurement: run with --ignored --nocapture"]
fn measure_side_index_lookups() {
    let fixture = Fixture::new();
    for sql in [
        "SELECT id, amount FROM events WHERE account = 17 ORDER BY id",
        "SELECT e.id FROM events e WHERE e.account IN (17, 508, 999) AND e.kind = 'open'",
        "SELECT COUNT(*), SUM(e.amount) FROM owners o JOIN events e ON e.account = o.id \
         WHERE o.id IN (17, 18, 508)",
    ] {
        for index in [false, true] {
            fixture.run(sql, index);
            let mut samples = (0..9)
                .map(|_| {
                    let started = std::time::Instant::now();
                    fixture.run(sql, index);
                    started.elapsed().as_secs_f64() * 1_000.0
                })
                .collect::<Vec<_>>();
            samples.sort_by(f64::total_cmp);
            println!(
                "{:>8.2} ms median  index={index}  {sql}",
                samples[samples.len() / 2]
            );
        }
    }
    let (entries, bytes, build_us) = pintail_store::side_index_totals();
    println!("side index: {entries} entries, {bytes} bytes, built in {build_us} us");
}

/// Rewrites every other flushed row and appends as many new ones again,
/// all left unflushed: 300,000 rows in the memtable over the segment.
fn fill_memtable(fixture: &mut Fixture) {
    let mut changes = Vec::new();
    for id in (2..=EVENTS).step_by(2) {
        fixture.version += 1;
        let mut event = fixture.model[&id].clone();
        event.amount += 1;
        changes.push(event_row(id, &event, fixture.version, false));
        fixture.model.insert(id, event);
    }
    for id in EVENTS + 1..=EVENTS + 200_000 {
        fixture.version += 1;
        let event = initial(id);
        changes.push(event_row(id, &event, fixture.version, false));
        fixture.model.insert(id, event);
    }
    for batch in changes.chunks(5_000) {
        fixture
            .events
            .ingest_cdc(batch.to_vec())
            .expect("change batch");
    }
}

#[test]
fn a_side_index_lookup_answers_exactly_over_a_large_memtable() {
    let mut fixture = Fixture::with_memtable_bytes(1 << 30);
    fill_memtable(&mut fixture);
    fixture.check("a large memtable");
}

#[test]
#[ignore = "measurement: run with --ignored --nocapture"]
fn measure_side_index_over_a_large_memtable() {
    let mut fixture = Fixture::with_memtable_bytes(1 << 30);
    fill_memtable(&mut fixture);
    for sql in [
        "SELECT id, amount FROM events WHERE account = 17 ORDER BY id",
        "SELECT e.id FROM events e WHERE e.account IN (17, 508, 999) AND e.kind = 'open'",
        "SELECT COUNT(*), SUM(e.amount) FROM owners o JOIN events e ON e.account = o.id \
         WHERE o.id IN (17, 18, 508)",
    ] {
        for index in [false, true] {
            fixture.run(sql, index);
            let mut samples = (0..9)
                .map(|_| {
                    let started = std::time::Instant::now();
                    fixture.run(sql, index);
                    started.elapsed().as_secs_f64() * 1_000.0
                })
                .collect::<Vec<_>>();
            samples.sort_by(f64::total_cmp);
            println!(
                "{:>8.2} ms median {:>8.2} ms min  memtable index={index}  {sql}",
                samples[samples.len() / 2],
                samples[0]
            );
        }
    }
}

/// Every row holding the smallest and the largest amount deleted, the
/// tombstones still in the memtable: the segment's postings place the
/// first rows among the deleted ones, too few rows come back from the
/// narrowed scan, and the sort must read the whole table instead.
#[test]
fn a_narrowed_order_limit_short_of_rows_reads_the_whole_table() {
    let mut fixture = Fixture::new();
    let doomed = fixture
        .model
        .iter()
        .filter(|(_, event)| matches!(event.amount, -40 | 60))
        .map(|(id, event)| (*id, event.clone()))
        .collect::<Vec<_>>();
    let mut changes = Vec::new();
    for (id, event) in doomed {
        fixture.version += 1;
        changes.push(event_row(id, &event, fixture.version, true));
        fixture.model.remove(&id);
    }
    for batch in changes.chunks(2_000) {
        fixture
            .events
            .ingest_cdc(batch.to_vec())
            .expect("delete batch");
    }
    fixture.check_order_limits("smallest and largest deleted");
    fixture.events.flush().expect("flush");
    fixture.check_order_limits("deletes flushed");
}

/// All but ten rows holding the smallest amount, and all but ten holding
/// the largest, moved to the middle, with new middle rows beside them, all
/// still in the memtable. The postings place the first rows among the moved
/// ones, so the narrowed scan holds ten rows at the bound; the rows it
/// returns from outside the bound must not make up the count, or the sort
/// answers with middle rows where the next value's rows belong.
#[test]
fn a_narrowed_order_limit_is_not_filled_from_outside_its_bound() {
    let mut fixture = Fixture::new();
    let extremes = fixture
        .model
        .iter()
        .filter(|(_, event)| matches!(event.amount, -40 | 60))
        .map(|(id, event)| (*id, event.clone()))
        .collect::<Vec<_>>();
    let mut changes = Vec::new();
    let mut kept = BTreeMap::new();
    for (id, event) in extremes {
        let seen = kept.entry(event.amount).or_insert(0_usize);
        *seen += 1;
        if *seen <= 10 {
            continue;
        }
        fixture.version += 1;
        let moved = Event {
            amount: 30,
            ..event
        };
        changes.push(event_row(id, &moved, fixture.version, false));
        fixture.model.insert(id, moved);
    }
    for id in EVENTS + 1..=EVENTS + 100 {
        fixture.version += 1;
        let event = Event {
            account: Some(5),
            kind: "held",
            amount: 0,
        };
        changes.push(event_row(id, &event, fixture.version, false));
        fixture.model.insert(id, event);
    }
    for batch in changes.chunks(2_000) {
        fixture
            .events
            .ingest_cdc(batch.to_vec())
            .expect("change batch");
    }
    fixture.check_order_limits("extremes moved, in the memtable");
    fixture.events.flush().expect("flush");
    fixture.check_order_limits("extremes moved, flushed");
}

#[test]
#[ignore = "measurement: run with --ignored --nocapture"]
fn measure_order_limits() {
    let fixture = Fixture::new();
    for sql in [
        "SELECT id, amount FROM events ORDER BY amount, id LIMIT 25",
        "SELECT id, kind FROM events ORDER BY amount DESC LIMIT 30",
        "SELECT id FROM events ORDER BY account DESC, id LIMIT 40",
        "SELECT id, note FROM events ORDER BY account DESC LIMIT 10",
    ] {
        for index in [false, true] {
            fixture.run(sql, index);
            let mut samples = (0..9)
                .map(|_| {
                    let started = std::time::Instant::now();
                    fixture.run(sql, index);
                    started.elapsed().as_secs_f64() * 1_000.0
                })
                .collect::<Vec<_>>();
            samples.sort_by(f64::total_cmp);
            println!(
                "{:>8.2} ms median {:>8.2} ms min  index={index}  {sql}",
                samples[samples.len() / 2],
                samples[0]
            );
        }
    }
}
