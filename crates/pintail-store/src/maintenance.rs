//! Background merges yield to the statements a server is answering.
//!
//! Every table merges on a thread of its own, started when its segments
//! call for it. Nothing used to bound what those threads cost: a replica
//! of many tables ran as many merges as it had tables behind, each at the
//! priority of the threads answering queries. Three bounds stand here.
//!
//! - **Width.** At most [`merge_threads`] merges run at once, process-wide;
//!   the others wait for a slot. A merge that has waited
//!   [`SLOT_WAIT_LIMIT`] runs anyway, so a slow merge delays its neighbours
//!   and never starves them.
//! - **Priority.** A merge thread asks the operating system to schedule it
//!   below the default, so a core a statement wants is the statement's.
//!   Where that is refused or not offered, the merge paces itself instead:
//!   while statements run it sleeps as long as it worked.
//! - **Writing.** While statements run, a merge writes at most
//!   [`merge_write_rate`] bytes a second.
//!
//! None of them may stop a merge. A pacing merge never sleeps longer than
//! it has worked, so it finishes in at most twice its unpaced time; and a
//! table that has fallen behind - a merge of
//! [`BEHIND_INPUTS`] segments or more - merges at full priority, unpaced, until it has
//! caught up.
//!
//! Statements are counted by whoever answers them
//! ([`statement_started`]); a process that never calls it has merges that
//! never pace, which is what an embedded store or a test wants.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Inputs from which one merge marks its table as behind: the planner
/// takes this many only when it widened a pass over a tier that flushes
/// outran, or when changes overlap this many segments at once.
pub(crate) const BEHIND_INPUTS: usize = 16;
/// The longest a merge waits for a slot before running without one.
const SLOT_WAIT_LIMIT: Duration = Duration::from_secs(30);
/// How long after a statement ends the process still counts as answering
/// statements: an application's statements arrive back to back, and the
/// gaps between them are not idleness.
const STATEMENT_LINGER: Duration = Duration::from_millis(5);
/// A merge sleeps once it owes this much, not for every row.
const SHORTEST_NAP: Duration = Duration::from_millis(2);
/// One sleep is at most this long, so a merge notices that statements
/// stopped or that it was asked to end.
const LONGEST_NAP: Duration = Duration::from_millis(100);
/// How far below the default a merge thread is scheduled.
#[cfg(target_os = "linux")]
const MERGE_NICENESS: i32 = 10;
const DEFAULT_WRITE_RATE: u64 = 96 * 1024 * 1024;
/// Rows a merge resolves between two looks at whether statements run.
const PACE_ROWS: usize = 1024;

static STATEMENTS: AtomicUsize = AtomicUsize::new(0);
/// Microseconds since [`epoch`] at which the last statement ended.
static LAST_STATEMENT: AtomicU64 = AtomicU64::new(0);
static RUNNING: AtomicUsize = AtomicUsize::new(0);
static WAITING: AtomicUsize = AtomicUsize::new(0);
static YIELDING: AtomicUsize = AtomicUsize::new(0);
static PENDING_BYTES: AtomicU64 = AtomicU64::new(0);
static YIELDED_MICROS: AtomicU64 = AtomicU64::new(0);
static BEHIND: AtomicUsize = AtomicUsize::new(0);
static COMPLETED: AtomicU64 = AtomicU64::new(0);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn micros_since_epoch() -> u64 {
    u64::try_from(epoch().elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Counts one statement as being answered until [`statement_finished`].
pub fn statement_started() {
    // The epoch is fixed before the first statement ends.
    let _ = epoch();
    STATEMENTS.fetch_add(1, Ordering::AcqRel);
}

/// Ends the count [`statement_started`] began.
pub fn statement_finished() {
    LAST_STATEMENT.store(micros_since_epoch().max(1), Ordering::Release);
    let _ = STATEMENTS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
        Some(count.saturating_sub(1))
    });
}

/// Whether a statement is being answered, or one ended a moment ago.
fn statements_active() -> bool {
    if STATEMENTS.load(Ordering::Acquire) > 0 {
        return true;
    }
    let last = LAST_STATEMENT.load(Ordering::Acquire);
    last != 0
        && micros_since_epoch().saturating_sub(last)
            < u64::try_from(STATEMENT_LINGER.as_micros()).unwrap_or(u64::MAX)
}

fn positive_setting(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
}

/// Merges that may run at once: `PINTAIL_MERGE_THREADS` when set, else a
/// quarter of the machine's threads, at least one and at most four. A
/// machine of four threads or fewer runs one.
#[must_use]
pub fn merge_threads() -> usize {
    static THREADS: OnceLock<usize> = OnceLock::new();
    *THREADS.get_or_init(|| {
        positive_setting("PINTAIL_MERGE_THREADS").map_or_else(
            || {
                let threads =
                    std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
                (threads / 4).clamp(1, 4)
            },
            |threads| usize::try_from(threads).unwrap_or(usize::MAX),
        )
    })
}

/// Bytes a second a merge writes while statements run:
/// `PINTAIL_MERGE_WRITE_BYTES_PER_SEC` when set.
#[must_use]
pub fn merge_write_rate() -> u64 {
    static RATE: OnceLock<u64> = OnceLock::new();
    *RATE.get_or_init(|| {
        positive_setting("PINTAIL_MERGE_WRITE_BYTES_PER_SEC").unwrap_or(DEFAULT_WRITE_RATE)
    })
}

/// What background maintenance is doing at this moment, process-wide.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MaintenanceStatus {
    /// Merges holding a slot and working.
    pub merges_running: usize,
    /// Merges started and waiting for a slot.
    pub merges_waiting: usize,
    /// Merges that may run at once.
    pub merge_threads: usize,
    /// Segment bytes the running and waiting merges have yet to finish.
    pub pending_bytes: u64,
    /// Statements being answered.
    pub statements_active: usize,
    /// Merges asleep at this moment because statements are running.
    pub merges_yielding: usize,
    /// Tables whose merges run unpaced because they fell behind.
    pub tables_behind: usize,
    /// Time merges have slept for statements since the process started.
    pub yielded_micros: u64,
    /// Merges finished since the process started.
    pub merges_completed: u64,
}

impl MaintenanceStatus {
    /// One word for the throttle: `idle` with no merge, `behind` when a
    /// table merges unpaced to catch up, `yielding` while merges give way
    /// to statements, `waiting` when every merge is queued for a slot, and
    /// `running` otherwise.
    #[must_use]
    pub fn throttle(&self) -> &'static str {
        if self.merges_running == 0 && self.merges_waiting == 0 {
            "idle"
        } else if self.tables_behind > 0 {
            "behind"
        } else if self.merges_yielding > 0
            || (self.statements_active > 0 && self.merges_running > 0)
        {
            "yielding"
        } else if self.merges_running == 0 {
            "waiting"
        } else {
            "running"
        }
    }
}

/// The process-wide state of background merges.
#[must_use]
pub fn maintenance_status() -> MaintenanceStatus {
    MaintenanceStatus {
        merges_running: RUNNING.load(Ordering::Acquire),
        merges_waiting: WAITING.load(Ordering::Acquire),
        merge_threads: merge_threads(),
        pending_bytes: PENDING_BYTES.load(Ordering::Acquire),
        statements_active: STATEMENTS.load(Ordering::Acquire),
        merges_yielding: YIELDING.load(Ordering::Acquire),
        tables_behind: BEHIND.load(Ordering::Acquire),
        yielded_micros: YIELDED_MICROS.load(Ordering::Acquire),
        merges_completed: COMPLETED.load(Ordering::Acquire),
    }
}

struct Slots {
    taken: Mutex<usize>,
    freed: Condvar,
}

fn slots() -> &'static Slots {
    static SLOTS: OnceLock<Slots> = OnceLock::new();
    SLOTS.get_or_init(|| Slots {
        taken: Mutex::new(0),
        freed: Condvar::new(),
    })
}

/// Asks the operating system to run the calling thread below the default
/// priority; `false` where it cannot or will not.
fn lower_thread_priority() -> bool {
    #[cfg(target_os = "linux")]
    {
        // On Linux a priority set for a thread's own ID is that thread's.
        rustix::process::setpriority_process(Some(rustix::thread::gettid()), MERGE_NICENESS).is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// One merge's place among the others, from the moment it is started to
/// the moment its thread ends: its bytes count as pending, it holds a slot
/// while it works, and it paces itself against the statements running.
pub(crate) struct MergeTicket {
    bytes: u64,
    /// The table fell behind: full priority, no pacing.
    behind: bool,
    slot: bool,
    /// The thread could not be scheduled below the default, so the merge
    /// gives way by sleeping.
    cooperative: bool,
    last: Instant,
    worked: Duration,
    slept: Duration,
    owed: Duration,
    /// Rows merged since the merge last looked at the statements running.
    rows: usize,
}

impl MergeTicket {
    /// Registers a merge of `bytes` of segments. Called by the thread that
    /// starts the merge, so its bytes are pending before its worker runs.
    pub(crate) fn queue(bytes: u64, behind: bool) -> Self {
        PENDING_BYTES.fetch_add(bytes, Ordering::AcqRel);
        WAITING.fetch_add(1, Ordering::AcqRel);
        if behind {
            BEHIND.fetch_add(1, Ordering::AcqRel);
        }
        Self {
            bytes,
            behind,
            slot: false,
            cooperative: true,
            last: Instant::now(),
            worked: Duration::ZERO,
            slept: Duration::ZERO,
            owed: Duration::ZERO,
            rows: 0,
        }
    }

    /// Waits for a slot on the merge's own thread, then lowers that
    /// thread's priority. Returns `false` when `stop` was set while
    /// waiting: the merge should end without working.
    pub(crate) fn admit(&mut self, stop: &AtomicBool) -> bool {
        let slots = slots();
        let limit = merge_threads();
        let started = Instant::now();
        let mut taken = slots.taken.lock().unwrap_or_else(PoisonError::into_inner);
        // A table that is behind does not queue behind the others.
        while *taken >= limit && !self.behind && started.elapsed() < SLOT_WAIT_LIMIT {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            taken = slots
                .freed
                .wait_timeout(taken, Duration::from_millis(50))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *taken += 1;
        drop(taken);
        self.slot = true;
        WAITING.fetch_sub(1, Ordering::AcqRel);
        RUNNING.fetch_add(1, Ordering::AcqRel);
        self.cooperative = self.behind || !lower_thread_priority();
        self.last = Instant::now();
        true
    }

    /// Counts one merged row, and gives way to running statements once
    /// every [`PACE_ROWS`] of them.
    pub(crate) fn pace_row(&mut self) {
        self.rows += 1;
        if self.rows >= PACE_ROWS {
            self.rows = 0;
            self.pace(0);
        }
    }

    /// Gives way to running statements, between pieces of work: `wrote`
    /// is the bytes written since the last call. Cheap when nothing runs.
    pub(crate) fn pace(&mut self, wrote: u64) {
        let now = Instant::now();
        let stretch = now.saturating_duration_since(self.last);
        self.worked = self.worked.saturating_add(stretch);
        self.last = now;
        if self.behind || !statements_active() {
            self.owed = Duration::ZERO;
            return;
        }
        if self.cooperative {
            self.owed = self.owed.saturating_add(stretch);
        }
        if wrote > 0 {
            let micros =
                u128::from(wrote).saturating_mul(1_000_000) / u128::from(merge_write_rate());
            self.owed = self.owed.saturating_add(Duration::from_micros(
                u64::try_from(micros).unwrap_or(u64::MAX),
            ));
        }
        // Never asleep longer than at work: a merge under statements that
        // never stop still ends in twice its own time.
        let allowed = self.worked.saturating_sub(self.slept);
        let nap = self.owed.min(allowed).min(LONGEST_NAP);
        if nap < SHORTEST_NAP {
            return;
        }
        YIELDING.fetch_add(1, Ordering::AcqRel);
        std::thread::sleep(nap);
        YIELDING.fetch_sub(1, Ordering::AcqRel);
        let slept = self.last.elapsed();
        self.slept = self.slept.saturating_add(slept);
        self.owed = self.owed.saturating_sub(nap);
        YIELDED_MICROS.fetch_add(
            u64::try_from(slept.as_micros()).unwrap_or(u64::MAX),
            Ordering::AcqRel,
        );
        self.last = Instant::now();
    }
}

impl Drop for MergeTicket {
    fn drop(&mut self) {
        PENDING_BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
        if self.behind {
            BEHIND.fetch_sub(1, Ordering::AcqRel);
        }
        if self.slot {
            RUNNING.fetch_sub(1, Ordering::AcqRel);
            COMPLETED.fetch_add(1, Ordering::AcqRel);
            let slots = slots();
            let mut taken = slots.taken.lock().unwrap_or_else(PoisonError::into_inner);
            *taken = taken.saturating_sub(1);
            drop(taken);
            slots.freed.notify_one();
        } else {
            WAITING.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pacing_merge_never_sleeps_longer_than_it_worked() {
        let mut ticket = MergeTicket::queue(1024, false);
        assert!(ticket.admit(&AtomicBool::new(false)));
        ticket.cooperative = true;
        statement_started();
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(3));
            // A write far past the rate asks for more sleep than is allowed.
            ticket.pace(merge_write_rate());
        }
        statement_finished();
        assert!(ticket.slept > Duration::ZERO, "a paced merge gives way");
        // Each sleep is asked for within what the merge has worked; the
        // clock may overshoot one by a scheduler tick.
        assert!(
            ticket.slept <= ticket.worked + Duration::from_millis(20),
            "slept {:?} for {:?} of work",
            ticket.slept,
            ticket.worked
        );
    }

    #[test]
    fn a_table_that_fell_behind_does_not_pace() {
        let mut ticket = MergeTicket::queue(1024, true);
        assert!(ticket.admit(&AtomicBool::new(false)));
        statement_started();
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(3));
            ticket.pace(merge_write_rate());
        }
        statement_finished();
        assert_eq!(ticket.slept, Duration::ZERO);
    }

    #[test]
    fn a_merge_asked_to_stop_leaves_the_queue() {
        let limit = merge_threads();
        let mut held = (0..limit)
            .map(|_| MergeTicket::queue(1, false))
            .collect::<Vec<_>>();
        for ticket in &mut held {
            // Other tests' tickets may hold slots: these wait their turn.
            assert!(ticket.admit(&AtomicBool::new(false)));
        }
        let mut waiting = MergeTicket::queue(7, false);
        assert!(!waiting.admit(&AtomicBool::new(true)));
        drop(waiting);
        drop(held);
    }
}
