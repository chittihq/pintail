//! One execution answers every identical request waiting on it.
//!
//! A dashboard refresh, several people opening the same report, a client
//! that retries: the server receives the same statement several times at
//! once and runs it several times. Each copy takes an admission permit, a
//! memory reservation and a full scan to produce rows byte for byte
//! identical to its neighbours'.
//!
//! This is not a cache. Nothing is kept: an entry lives only while its
//! execution runs, and the next request executes again. That is what makes
//! it safe to reason about - there is no window in which stored rows and
//! the store disagree, because nothing is stored. Correctness reduces to
//! one question, and it is a question about a struct rather than about a
//! schedule: does [`SharedQueryKey`] name everything that could make two
//! executions of the same text differ?
//!
//! What the key names, and why each is there:
//!
//! - the loaded replica, by a number taken fresh on every load, so a CDC
//!   commit or a local write puts the same text on a different key rather
//!   than answering it from the snapshot before the change;
//! - the statement text, which carries its own parameters (the wire server
//!   substitutes them before executing) and its own hints;
//! - the row ceiling, which decides where a result is truncated;
//! - the session settings an execution reads: the zone its clock functions
//!   observe, the collation its comparisons follow, and the two caps that
//!   change values rather than only rejecting them.
//!
//! The database is inside the replica number, and every caller has already
//! passed its own authorization for that database before reaching here.
//!
//! Two things are deliberately not shared. A failure is never handed to a
//! waiter - if the execution errors, is cancelled by its own client, or
//! panics, everyone waiting executes independently, exactly as they would
//! have without this module. And a statement whose answer could differ
//! between two runs never enters (`pintail_sql::is_repeatable_statement`).

use std::{
    collections::HashMap,
    sync::{
        Arc, Condvar, LazyLock, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::engine::QueryOutput;

/// Statements executing under this coordinator at one time. Beyond it a
/// request executes independently: the bound exists so a server under a
/// flood of distinct statements spends nothing on bookkeeping it cannot
/// use.
const MAX_FLIGHTS: usize = 64;

/// Requests that may wait on one execution. A result is handed out by
/// clone, so a very wide fan-out would turn one execution's memory into
/// many; past this a request executes on its own instead.
const MAX_FOLLOWERS: usize = 64;

/// How long a waiter sleeps before rechecking its own deadline.
const WAIT_SLICE: Duration = Duration::from_millis(25);

/// `PINTAIL_DISABLE_SHARED_QUERIES` puts every request back on its own
/// execution. It is how the measurement runs both arms, and it is the
/// switch to reach for if a deployment ever needs one request to mean one
/// execution.
static DISABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("PINTAIL_DISABLE_SHARED_QUERIES").is_some());

/// Everything that must match before one execution can answer another
/// request. See the module documentation for why each field is here;
/// adding an input to execution without adding it here is the one way this
/// module returns a wrong answer.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SharedQueryKey {
    /// Identifies one load of one database's replica. Never reused.
    pub(crate) replica: u64,
    pub(crate) sql: String,
    pub(crate) max_rows: usize,
    pub(crate) time_zone: Option<String>,
    pub(crate) collation: &'static str,
    pub(crate) group_concat_max_len: usize,
    pub(crate) cte_max_recursion_depth: u64,
    /// The session's `sql_mode` flags: the same text binds to a different
    /// statement, or evaluates differently, under another mode.
    pub(crate) parse_mode: pintail_sql::ParseMode,
}

impl SharedQueryKey {
    /// The key for `sql` against `replica`, reading the session settings
    /// installed on the calling thread - the same thread the execution will
    /// run on, so what is read is what the execution will observe.
    pub(crate) fn for_current_session(replica: u64, sql: &str, max_rows: usize) -> Self {
        Self {
            replica,
            sql: sql.to_owned(),
            max_rows,
            time_zone: pintail_exec::session_time_zone_key(),
            collation: pintail_sql::session_default_collation(),
            group_concat_max_len: pintail_exec::session_group_concat_max_len(),
            cte_max_recursion_depth: pintail_exec::session_cte_max_recursion_depth(),
            parse_mode: pintail_sql::session_parse_mode(),
        }
    }
}

/// What a request found when it offered to share.
pub(crate) enum Join {
    /// Nobody was running this. The caller executes and must report the
    /// outcome through the returned guard.
    Lead(Leader),
    /// Another execution produced this answer.
    Followed(Arc<QueryOutput>),
    /// Execute independently and report nothing: the statement was not
    /// eligible, the coordinator was full, or the execution that was
    /// running did not succeed.
    Alone,
}

/// What a flight has produced so far.
enum Outcome {
    Running,
    /// Success, the only outcome a waiter is given.
    Ready(Arc<QueryOutput>),
    /// Error, cancellation or panic. Waiters execute independently.
    Failed,
}

struct FlightState {
    outcome: Outcome,
    followers: usize,
}

struct Flight {
    state: Mutex<FlightState>,
    ready: Condvar,
}

/// What the coordinator has done since the process started.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SharedQueryStats {
    /// Executions that led a flight, whether or not anyone joined.
    pub led: u64,
    /// Requests answered by another request's execution. This is the number
    /// of executions, admission permits and scans that did not happen.
    pub followed: u64,
    /// Requests that waited and then executed anyway: the leader failed,
    /// was cancelled, or did not finish inside the waiter's own deadline.
    pub fell_back: u64,
    /// Requests that could not be offered a flight because the coordinator
    /// or the flight was at its bound.
    pub refused: u64,
}

#[derive(Default)]
struct Counters {
    led: AtomicU64,
    followed: AtomicU64,
    fell_back: AtomicU64,
    refused: AtomicU64,
}

/// Collapses concurrent identical executions.
pub(crate) struct SharedQueries {
    flights: Mutex<HashMap<SharedQueryKey, Arc<Flight>>>,
    counters: Counters,
}

impl SharedQueries {
    fn new() -> Self {
        Self {
            flights: Mutex::new(HashMap::new()),
            counters: Counters::default(),
        }
    }

    /// Offers to share, and either takes the lead, waits out an execution
    /// already running, or declines.
    ///
    /// `deadline` is the caller's own. A waiter never sleeps past it: it
    /// stops waiting and executes independently, so a client with a tight
    /// `max_execution_time` is not held to a leader's looser one.
    pub(crate) fn join(self: &Arc<Self>, key: &SharedQueryKey, deadline: Option<Instant>) -> Join {
        if *DISABLED {
            return Join::Alone;
        }
        let flight = {
            let mut flights = self
                .flights
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(existing) = flights.get(key) else {
                if flights.len() >= MAX_FLIGHTS {
                    self.counters.refused.fetch_add(1, Ordering::Relaxed);
                    return Join::Alone;
                }
                let flight = Arc::new(Flight {
                    state: Mutex::new(FlightState {
                        outcome: Outcome::Running,
                        followers: 0,
                    }),
                    ready: Condvar::new(),
                });
                flights.insert(key.clone(), Arc::clone(&flight));
                self.counters.led.fetch_add(1, Ordering::Relaxed);
                return Join::Lead(Leader {
                    coordinator: Arc::clone(self),
                    key: key.clone(),
                    flight,
                    finished: false,
                });
            };
            let existing = Arc::clone(existing);
            let mut state = existing
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.followers >= MAX_FOLLOWERS {
                self.counters.refused.fetch_add(1, Ordering::Relaxed);
                return Join::Alone;
            }
            state.followers += 1;
            drop(state);
            existing
        };
        self.wait(&flight, deadline)
    }

    fn wait(&self, flight: &Arc<Flight>, deadline: Option<Instant>) -> Join {
        let mut state = flight
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            match &state.outcome {
                Outcome::Ready(output) => {
                    let output = Arc::clone(output);
                    state.followers -= 1;
                    self.counters.followed.fetch_add(1, Ordering::Relaxed);
                    return Join::Followed(output);
                }
                Outcome::Failed => {
                    state.followers -= 1;
                    self.counters.fell_back.fetch_add(1, Ordering::Relaxed);
                    return Join::Alone;
                }
                Outcome::Running => {}
            }
            // A deadline that has already passed leaves immediately; the
            // caller's own execution will report the interruption, so this
            // module never invents one.
            let slice = match deadline {
                Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                    None => {
                        state.followers -= 1;
                        self.counters.fell_back.fetch_add(1, Ordering::Relaxed);
                        return Join::Alone;
                    }
                    Some(remaining) => remaining.min(WAIT_SLICE),
                },
                None => WAIT_SLICE,
            };
            let (guard, _timeout) = flight
                .ready
                .wait_timeout(state, slice)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = guard;
        }
    }

    /// Publishes an outcome and wakes everyone waiting on it.
    fn settle(&self, key: &SharedQueryKey, flight: &Arc<Flight>, outcome: Outcome) {
        // Removed from the map first, so a request arriving after this
        // point starts a new flight rather than joining a finished one.
        self.flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
        let mut state = flight
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.outcome = outcome;
        drop(state);
        flight.ready.notify_all();
    }

    /// How many requests are waiting on `key`, for the tests that assert
    /// the fan-out bound.
    #[cfg(test)]
    fn followers(&self, key: &SharedQueryKey) -> usize {
        let flights = self
            .flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        flights.get(key).map_or(0, |flight| {
            flight
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .followers
        })
    }

    /// What this coordinator has done since the process started.
    fn stats(&self) -> SharedQueryStats {
        SharedQueryStats {
            led: self.counters.led.load(Ordering::Relaxed),
            followed: self.counters.followed.load(Ordering::Relaxed),
            fell_back: self.counters.fell_back.load(Ordering::Relaxed),
            refused: self.counters.refused.load(Ordering::Relaxed),
        }
    }
}

/// The request that is executing on everyone else's behalf.
///
/// Dropping without [`Leader::succeeded`] settles the flight as failed, so
/// a panic or an early return wakes waiters to execute for themselves
/// rather than leaving them asleep until their deadlines.
pub(crate) struct Leader {
    coordinator: Arc<SharedQueries>,
    key: SharedQueryKey,
    flight: Arc<Flight>,
    finished: bool,
}

impl Leader {
    /// Hands this output to everyone waiting.
    pub(crate) fn succeeded(mut self, output: &QueryOutput) {
        self.finished = true;
        self.coordinator.settle(
            &self.key,
            &self.flight,
            Outcome::Ready(Arc::new(output.clone())),
        );
    }
}

impl Drop for Leader {
    fn drop(&mut self) {
        if !self.finished {
            self.coordinator
                .settle(&self.key, &self.flight, Outcome::Failed);
        }
    }
}

static SHARED_QUERIES: OnceLock<Arc<SharedQueries>> = OnceLock::new();

/// The coordinator every engine in the process shares.
pub(crate) fn shared_queries() -> &'static Arc<SharedQueries> {
    SHARED_QUERIES.get_or_init(|| Arc::new(SharedQueries::new()))
}

/// What the shared-execution coordinator has done since startup.
#[must_use]
pub fn shared_query_stats() -> SharedQueryStats {
    shared_queries().stats()
}

#[cfg(test)]
mod tests {
    use super::{Join, MAX_FLIGHTS, MAX_FOLLOWERS, SharedQueries, SharedQueryKey};
    use crate::engine::{QueryOutput, QueryStats};
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    /// Each test works in its own coordinator: the shared one is
    /// process-wide, and tests that filled it would refuse each other's
    /// flights rather than test their own behaviour.
    fn coordinator() -> Arc<SharedQueries> {
        Arc::new(SharedQueries::new())
    }

    fn key(sql: &str) -> SharedQueryKey {
        SharedQueryKey {
            replica: 1,
            sql: sql.to_owned(),
            max_rows: 100,
            time_zone: None,
            collation: "utf8mb4_0900_ai_ci",
            group_concat_max_len: 1024,
            cte_max_recursion_depth: 1000,
            parse_mode: pintail_sql::ParseMode::default(),
        }
    }

    fn output(tag: i64) -> QueryOutput {
        QueryOutput {
            fields: Vec::new(),
            rows: vec![vec![pintail_types::Value::Int64(tag)]],
            stats: QueryStats::default(),
            truncated: false,
            affected: None,
        }
    }

    /// Blocks until `predicate` holds, or fails the test.
    fn until(what: &str, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_waiter_receives_the_running_execution_s_answer() {
        let coordinator = coordinator();
        let key = key("SELECT total FROM orders");
        let Join::Lead(leader) = coordinator.join(&key, None) else {
            panic!("the first request leads");
        };
        let waiter = std::thread::spawn({
            let (coordinator, key) = (Arc::clone(&coordinator), key.clone());
            move || coordinator.join(&key, None)
        });
        // Publishing only once the waiter is parked is what makes this a
        // test of waiting rather than of arriving late.
        until("the waiter to park", || coordinator.followers(&key) == 1);
        leader.succeeded(&output(7));

        let Join::Followed(shared) = waiter.join().expect("waiter") else {
            panic!("the waiter is answered by the execution it waited for");
        };
        assert_eq!(shared.rows, output(7).rows);
        assert_eq!(coordinator.stats().followed, 1);
        assert_eq!(coordinator.stats().led, 1, "one execution, two requests");
    }

    #[test]
    fn nothing_is_kept_after_the_execution_finishes() {
        let coordinator = coordinator();
        let key = key("SELECT 1");
        let Join::Lead(leader) = coordinator.join(&key, None) else {
            panic!("leads");
        };
        leader.succeeded(&output(1));
        // This coordinator holds an execution, never a result: there is no
        // window in which a kept answer and the store could disagree,
        // because nothing is kept.
        assert!(matches!(coordinator.join(&key, None), Join::Lead(_)));
    }

    #[test]
    fn every_field_of_the_key_separates_two_requests() {
        let coordinator = coordinator();
        let base = key("SELECT region, SUM(total) FROM orders GROUP BY region");
        let variants = [
            SharedQueryKey {
                replica: base.replica + 1,
                ..base.clone()
            },
            SharedQueryKey {
                sql: "SELECT region FROM orders".to_owned(),
                ..base.clone()
            },
            SharedQueryKey {
                max_rows: 101,
                ..base.clone()
            },
            SharedQueryKey {
                time_zone: Some("f19800".to_owned()),
                ..base.clone()
            },
            SharedQueryKey {
                collation: "utf8mb4_bin",
                ..base.clone()
            },
            SharedQueryKey {
                group_concat_max_len: 4096,
                ..base.clone()
            },
            SharedQueryKey {
                cte_max_recursion_depth: 10,
                ..base.clone()
            },
            SharedQueryKey {
                parse_mode: pintail_sql::ParseMode {
                    no_unsigned_subtraction: true,
                    ..pintail_sql::ParseMode::default()
                },
                ..base.clone()
            },
        ];
        let Join::Lead(_leader) = coordinator.join(&base, None) else {
            panic!("leads");
        };
        for variant in variants {
            assert_ne!(variant, base);
            assert!(
                matches!(coordinator.join(&variant, None), Join::Lead(_)),
                "a request differing in one field must not join: {variant:?}"
            );
        }
    }

    #[test]
    fn a_failed_execution_sends_its_waiters_to_run_for_themselves() {
        let coordinator = coordinator();
        let key = key("SELECT 2");
        let Join::Lead(leader) = coordinator.join(&key, None) else {
            panic!("leads");
        };
        let waiter = std::thread::spawn({
            let (coordinator, key) = (Arc::clone(&coordinator), key.clone());
            move || coordinator.join(&key, None)
        });
        until("the waiter to park", || coordinator.followers(&key) == 1);
        // Dropped without succeeding: the shape an error, a cancellation by
        // the leader's own client, or a panic in its execution takes. A
        // failure is never handed to a waiter.
        drop(leader);
        assert!(matches!(waiter.join().expect("waiter"), Join::Alone));
        assert_eq!(coordinator.stats().followed, 0);
        assert_eq!(coordinator.stats().fell_back, 1);
    }

    #[test]
    fn a_panicking_leader_still_wakes_its_waiters() {
        let coordinator = coordinator();
        let key = key("SELECT 3");
        let panicking = std::thread::spawn({
            let (coordinator, key) = (Arc::clone(&coordinator), key.clone());
            move || {
                let Join::Lead(_leader) = coordinator.join(&key, None) else {
                    panic!("leads");
                };
                panic!("the execution panics");
            }
        });
        assert!(panicking.join().is_err());
        // Settled by the guard's drop during unwinding, so the next request
        // leads rather than waiting on an execution that will never finish.
        assert!(matches!(coordinator.join(&key, None), Join::Lead(_)));
    }

    #[test]
    fn a_waiter_never_sleeps_past_its_own_deadline() {
        let coordinator = coordinator();
        let key = key("SELECT 4");
        let Join::Lead(_leader) = coordinator.join(&key, None) else {
            panic!("leads");
        };
        let started = Instant::now();
        let joined = coordinator.join(&key, Some(started + Duration::from_millis(60)));
        let waited = started.elapsed();
        assert!(matches!(joined, Join::Alone));
        assert!(
            waited < Duration::from_secs(2),
            "a client with a tight max_execution_time is not held to the leader's looser one, \
             waited {waited:?}"
        );
        assert_eq!(coordinator.stats().fell_back, 1);
    }

    #[test]
    fn a_deadline_already_past_does_not_wait_at_all() {
        let coordinator = coordinator();
        let key = key("SELECT 5");
        let Join::Lead(_leader) = coordinator.join(&key, None) else {
            panic!("leads");
        };
        let started = Instant::now();
        assert!(matches!(
            coordinator.join(
                &key,
                Some(
                    started
                        .checked_sub(Duration::from_secs(1))
                        .unwrap_or(started)
                )
            ),
            Join::Alone
        ));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn a_flight_stops_taking_waiters_at_its_bound() {
        let coordinator = coordinator();
        let key = key("SELECT 6");
        let Join::Lead(leader) = coordinator.join(&key, None) else {
            panic!("leads");
        };
        let release = Arc::new(Barrier::new(MAX_FOLLOWERS + 1));
        let waiters = (0..MAX_FOLLOWERS)
            .map(|_| {
                let (coordinator, key, release) =
                    (Arc::clone(&coordinator), key.clone(), Arc::clone(&release));
                std::thread::spawn(move || {
                    release.wait();
                    coordinator.join(&key, Some(Instant::now() + Duration::from_secs(30)))
                })
            })
            .collect::<Vec<_>>();
        release.wait();
        until("every waiter to park", || {
            coordinator.followers(&key) == MAX_FOLLOWERS
        });
        // One more executes on its own rather than growing the fan-out.
        assert!(matches!(coordinator.join(&key, None), Join::Alone));
        assert_eq!(coordinator.stats().refused, 1);

        leader.succeeded(&output(9));
        for waiter in waiters {
            assert!(matches!(waiter.join().expect("waiter"), Join::Followed(_)));
        }
    }

    #[test]
    fn the_coordinator_declines_rather_than_grow_without_bound() {
        let coordinator = coordinator();
        let leaders = (0..MAX_FLIGHTS)
            .map(|index| {
                let key = key(&format!("SELECT {index} FROM orders"));
                match coordinator.join(&key, None) {
                    Join::Lead(leader) => leader,
                    _ => panic!("each distinct statement leads its own flight"),
                }
            })
            .collect::<Vec<_>>();
        // With the map full, an unrelated statement executes on its own
        // rather than displacing an execution someone is waiting on.
        assert!(matches!(
            coordinator.join(&key("SELECT beyond FROM orders"), None),
            Join::Alone
        ));
        assert_eq!(coordinator.stats().refused, 1);
        drop(leaders);
    }
}
