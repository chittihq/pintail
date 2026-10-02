//! A statement folding its scan in place stops when it is killed or runs
//! past its deadline, with the error the pulled path gives.
//!
//! Nothing here depends on timing. The scans are wrapped: once a statement
//! has taken a set number of fused rounds - or, with the rounds withheld so
//! the statement pulls, a set number of batches - the wrapper cancels the
//! statement, or waits its deadline out. The next read must then fail, and
//! the rounds the wrapper counted show the statement was folding in place
//! when it was stopped.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    BatchStream, ExecError, Execution, ExecutionCancellation, FoldedRound, LogicalPlanner,
    Optimizer, PhysicalPlanner, RecordBatch, Scan, ScanBatchFold, ScanBatchPlace, ScanProvider,
    SnapshotScanProvider, with_execution_cancellation,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const VISITS: u64 = 600_000;
const ACCOUNTS: u64 = 5_000;

/// How the wrapped scans are read, and what happens once the statement has
/// read `after` units that way.
struct Trip {
    /// Whether the scans fold in place; otherwise every round is withheld
    /// and the statement pulls batches.
    fused: bool,
    after: usize,
    seen: AtomicUsize,
    rounds: AtomicUsize,
    action: Box<dyn Fn() + Send + Sync>,
    /// The threads that folded a batch of a round, when the run asks for
    /// the first of them to wait for a second.
    meeting: Option<Mutex<HashSet<ThreadId>>>,
}

impl Trip {
    fn step(&self) {
        if self.seen.fetch_add(1, Ordering::SeqCst) + 1 == self.after {
            (self.action)();
        }
    }
}

struct TrippedStream {
    inner: Box<dyn BatchStream>,
    trip: Arc<Trip>,
}

impl BatchStream for TrippedStream {
    fn next_batch(&mut self, available_memory: usize) -> Result<Option<RecordBatch>, ExecError> {
        let batch = self.inner.next_batch(available_memory)?;
        if !self.trip.fused && batch.is_some() {
            self.trip.step();
        }
        Ok(batch)
    }

    fn retained_bytes(&self) -> usize {
        self.inner.retained_bytes()
    }

    fn next_batch_memory_upper_bound(&self, budget: usize) -> usize {
        self.inner.next_batch_memory_upper_bound(budget)
    }

    fn prefilter_collation(&self) -> Option<Collation> {
        self.inner.prefilter_collation()
    }

    fn last_batch_prefiltered(&self) -> bool {
        self.inner.last_batch_prefiltered()
    }

    fn fold_round(
        &mut self,
        available_memory: usize,
        max_batches: usize,
        fold: ScanBatchFold<'_>,
    ) -> Result<FoldedRound, ExecError> {
        if !self.trip.fused {
            return Ok(FoldedRound::Unavailable);
        }
        let round = match &self.trip.meeting {
            None => self.inner.fold_round(available_memory, max_batches, fold)?,
            Some(meeting) => {
                let met = |batch: RecordBatch, place: ScanBatchPlace| {
                    let first = {
                        let mut threads = meeting.lock().expect("meeting");
                        threads.insert(std::thread::current().id());
                        threads.len() == 1
                    };
                    // The first thread waits for a second to fold a batch
                    // of the same round, which only a round on several
                    // workers at once provides. The wait is bounded so a
                    // serial round fails the test rather than hanging it.
                    let until = Instant::now() + Duration::from_secs(20);
                    while first
                        && meeting.lock().expect("meeting").len() < 2
                        && Instant::now() < until
                    {
                        std::thread::yield_now();
                    }
                    fold(batch, place)
                };
                self.inner.fold_round(available_memory, max_batches, &met)?
            }
        };
        if matches!(round, FoldedRound::Round { .. }) {
            self.trip.rounds.fetch_add(1, Ordering::SeqCst);
            self.trip.step();
        }
        Ok(round)
    }
}

struct TrippedProvider<'a> {
    inner: SnapshotScanProvider<'a>,
    trip: Arc<Trip>,
}

impl ScanProvider for TrippedProvider<'_> {
    fn open_scan(
        &self,
        scan: &Scan,
        memory_limit: usize,
    ) -> Result<Box<dyn BatchStream>, ExecError> {
        Ok(Box::new(TrippedStream {
            inner: self.inner.open_scan(scan, memory_limit)?,
            trip: Arc::clone(&self.trip),
        }))
    }
}

struct Fixture {
    _directories: [tempfile::TempDir; 2],
    visits: TableStore,
    accounts: TableStore,
    catalog: CatalogSnapshot,
}

fn key(id: u64) -> PrimaryKey {
    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key")
}

fn ingest_visits(visits: &mut TableStore) {
    let mut start = 0;
    while start < VISITS {
        let end = (start + 40_000).min(VISITS);
        visits
            .bulk_ingest_snapshot(
                (start..end)
                    .map(|id| {
                        StoredRow::new(
                            key(id),
                            vec![
                                Value::UInt64(id),
                                Value::UInt64(id % ACCOUNTS),
                                Value::Utf8(["open", "held", "done"][(id % 3) as usize].to_owned()),
                                Value::Utf8(format!("{}.{:02}", id % 900, id % 100)),
                            ],
                            1,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("ingest");
        start = end;
    }
}

fn fixture() -> Fixture {
    let visits_schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "account", DataType::UInt64, false),
            Column::new(3, "state", DataType::Utf8, false),
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
    .expect("schema");
    let accounts_schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "tier", DataType::Utf8, false),
        ],
    )
    .expect("schema");
    let directories = [
        tempfile::tempdir().expect("directory"),
        tempfile::tempdir().expect("directory"),
    ];
    let mut visits = TableStore::open(
        directories[0].path(),
        visits_schema.clone(),
        StoreOptions::default(),
    )
    .expect("table");
    ingest_visits(&mut visits);
    let mut accounts = TableStore::open(
        directories[1].path(),
        accounts_schema.clone(),
        StoreOptions::default(),
    )
    .expect("table");
    accounts
        .bulk_ingest_snapshot(
            (0..ACCOUNTS)
                .map(|id| {
                    StoredRow::new(
                        key(id),
                        vec![
                            Value::UInt64(id),
                            Value::Utf8(
                                ["free", "paid", "team", "firm"][(id % 4) as usize].to_owned(),
                            ),
                        ],
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let entry = |id, name: &str, schema, rows| {
        TableEntry::new(
            TableId::new(id),
            name,
            schema,
            TableStatistics::with_row_count(rows),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key")
    };
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        DatabaseId::new(1),
        "app",
        [
            entry(1, "visits", visits_schema, VISITS),
            entry(2, "accounts", accounts_schema, ACCOUNTS),
        ],
    )
    .expect("database")])
    .expect("catalog");
    Fixture {
        _directories: directories,
        visits,
        accounts,
        catalog,
    }
}

/// The three forms that fold in place: a grouped aggregate, an aggregate
/// over a join, and an ungrouped one.
const GROUPED: &str = "SELECT state, COUNT(*), SUM(amount) FROM visits GROUP BY state";
const JOINED: &str = "SELECT a.tier, COUNT(*), SUM(v.amount) FROM visits v \
                      JOIN accounts a ON v.account = a.id GROUP BY a.tier";
const UNGROUPED: &str = "SELECT MIN(amount), MAX(amount), SUM(amount) FROM visits";

/// Runs `sql` to its end over scans wrapped in `trip`: the rows it
/// returned, or the error that ended it.
fn run(
    fixture: &Fixture,
    sql: &str,
    trip: &Arc<Trip>,
    deadline: Option<Instant>,
) -> Result<usize, ExecError> {
    let visits = fixture.visits.snapshot();
    let accounts = fixture.accounts.snapshot();
    let provider = TrippedProvider {
        inner: SnapshotScanProvider::new([
            (DatabaseId::new(1), TableId::new(1), &visits),
            (DatabaseId::new(1), TableId::new(2), &accounts),
        ])
        .expect("provider"),
        trip: Arc::clone(trip),
    };
    let bound = Binder::new(&fixture.catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start_with_deadline(plan, &provider, 256 << 20, deadline, Collation::default())?;
    let mut rows = 0;
    while let Some(batch) = execution.next_batch()? {
        rows += batch.visible_row_count();
    }
    Ok(rows)
}

fn trip(fused: bool, after: usize, action: impl Fn() + Send + Sync + 'static) -> Arc<Trip> {
    Arc::new(Trip {
        fused,
        after,
        seen: AtomicUsize::new(0),
        rounds: AtomicUsize::new(0),
        action: Box::new(action),
        meeting: None,
    })
}

/// Whether this process folds in place at all.
fn fused() -> bool {
    std::env::var_os("PINTAIL_DISABLE_FUSED_FOLD").is_none()
}

#[test]
fn a_kill_stops_a_statement_between_fused_rounds_as_it_stops_a_pulled_one() {
    let fixture = fixture();
    for sql in [GROUPED, JOINED, UNGROUPED] {
        for in_place in [true, false] {
            let cancellation = ExecutionCancellation::new();
            let handle = cancellation.clone();
            let trip = trip(in_place, 1, move || handle.cancel());
            let outcome =
                with_execution_cancellation(cancellation, || run(&fixture, sql, &trip, None));
            if in_place && !fused() {
                // The switch keeps the statement pulling: nothing trips.
                assert!(outcome.is_ok(), "{sql}: {outcome:?}");
                continue;
            }
            assert!(
                matches!(outcome, Err(ExecError::QueryCancelled)),
                "{sql}, in place {in_place}: {outcome:?}"
            );
            assert_eq!(
                trip.rounds.load(Ordering::SeqCst) >= 1,
                in_place,
                "{sql}: the kill landed between fused rounds only when folding in place"
            );
        }
    }
}

#[test]
fn a_deadline_stops_a_statement_between_fused_rounds_as_it_stops_a_pulled_one() {
    let fixture = fixture();
    for sql in [GROUPED, JOINED, UNGROUPED] {
        for in_place in [true, false] {
            // Far enough off that the statement is reading when the wrapper
            // waits it out, however slow the build under test is.
            let deadline = Instant::now() + Duration::from_secs(4);
            let trip = trip(in_place, 1, move || {
                while Instant::now() <= deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
            let outcome = run(&fixture, sql, &trip, Some(deadline));
            if in_place && !fused() {
                assert!(outcome.is_ok(), "{sql}: {outcome:?}");
                continue;
            }
            assert!(
                matches!(outcome, Err(ExecError::QueryTimedOut)),
                "{sql}, in place {in_place}: {outcome:?}"
            );
            assert_eq!(
                trip.rounds.load(Ordering::SeqCst) >= 1,
                in_place,
                "{sql}: the deadline passed between fused rounds only when folding in place"
            );
        }
    }
}

#[test]
fn an_untripped_statement_answers_and_a_join_counts_its_fused_rounds() {
    let fixture = fixture();
    for (sql, groups) in [(GROUPED, 3), (JOINED, 4), (UNGROUPED, 1)] {
        let untripped = trip(true, usize::MAX, || {});
        let _ = pintail_exec::take_exec_counters();
        assert_eq!(
            run(&fixture, sql, &untripped, None).expect("answer"),
            groups,
            "{sql}"
        );
        let counters = pintail_exec::take_exec_counters();
        if fused() {
            assert!(
                counters.fused_rounds > 0 && untripped.rounds.load(Ordering::SeqCst) > 0,
                "{sql}: {counters:?}"
            );
            if sql != UNGROUPED {
                assert!(counters.fused_batches > 0, "{sql}: {counters:?}");
            }
        }
        // The same statement pulled gives the same number of groups.
        let pulled = trip(false, usize::MAX, || {});
        assert_eq!(
            run(&fixture, sql, &pulled, None).expect("answer"),
            groups,
            "{sql}"
        );
    }
}

/// Two workers fold batches of one round at the same time: the first thread
/// to fold holds its batch until a second thread folds one too. A round that
/// ran on one thread would never bring the second, and the bounded wait would
/// leave one thread on record.
#[test]
fn a_fused_round_folds_on_several_workers_at_once() {
    if !fused() || pintail_exec::parallel_pool_threads().0 < 2 {
        return;
    }
    let fixture = fixture();
    for (sql, groups) in [(GROUPED, 3), (JOINED, 4), (UNGROUPED, 1)] {
        let meeting = Arc::new(Trip {
            fused: true,
            after: usize::MAX,
            seen: AtomicUsize::new(0),
            rounds: AtomicUsize::new(0),
            action: Box::new(|| {}),
            meeting: Some(Mutex::new(HashSet::new())),
        });
        assert_eq!(
            run(&fixture, sql, &meeting, None).expect("answer"),
            groups,
            "{sql}"
        );
        let threads = meeting
            .meeting
            .as_ref()
            .map_or(0, |threads| threads.lock().expect("meeting").len());
        assert!(threads >= 2, "{sql}: folded on {threads} thread(s)");
    }
}
