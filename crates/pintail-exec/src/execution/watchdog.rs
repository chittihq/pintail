//! Query lifetime and aggregate reservation accounting for pressure cancellation.
use std::{
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

#[derive(Debug)]
struct QueryState {
    cancelled: AtomicBool,
    /// The query's deadline as nanoseconds after [`deadline_epoch`];
    /// `u64::MAX` when it has none.
    deadline: AtomicU64,
    bytes: AtomicUsize,
    /// Blocked waiting for the shared budget to release memory.
    waiting: AtomicBool,
    /// Registration order: a larger number is a younger query.
    sequence: u64,
}

impl Default for QueryState {
    fn default() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            deadline: AtomicU64::new(u64::MAX),
            bytes: AtomicUsize::new(0),
            waiting: AtomicBool::new(false),
            sequence: 0,
        }
    }
}

/// The instant deadlines are counted from, so one fits in an atomic.
fn deadline_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Cooperative cancellation shared by all trackers belonging to one query.
#[derive(Clone, Debug)]
pub struct ExecutionCancellation {
    state: Arc<QueryState>,
}

impl Default for ExecutionCancellation {
    fn default() -> Self {
        registry().register()
    }
}

impl ExecutionCancellation {
    /// Creates a live cancellation handle registered with the watchdog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cooperative cancellation at the next operator check.
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// Gives the query a deadline, or an earlier one than it had.
    ///
    /// The execution's trackers check their own deadline between batches;
    /// this one is for loops inside one value's evaluation, which reach
    /// the query only through the thread's handle.
    pub fn limit_to(&self, deadline: Instant) {
        let nanos = deadline
            .saturating_duration_since(deadline_epoch())
            .as_nanos();
        let nanos = u64::try_from(nanos).unwrap_or(u64::MAX - 1);
        self.state.deadline.fetch_min(nanos, Ordering::AcqRel);
    }

    /// Why the query should stop now: cancelled, or past its deadline.
    pub(crate) fn interruption(&self) -> Result<(), super::ExecError> {
        if self.is_cancelled() {
            return Err(super::ExecError::QueryCancelled);
        }
        let deadline = self.state.deadline.load(Ordering::Acquire);
        if deadline != u64::MAX && deadline_epoch().elapsed().as_nanos() >= u128::from(deadline) {
            return Err(super::ExecError::QueryTimedOut);
        }
        Ok(())
    }

    pub(super) fn reserve(&self, bytes: usize) {
        self.state.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
    pub(super) fn release(&self, bytes: usize) {
        self.state.bytes.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// Marks the query as blocked on the shared budget, or no longer.
    pub(super) fn set_waiting(&self, waiting: bool) {
        self.state.waiting.store(waiting, Ordering::Release);
    }

    /// Whether this query must give way on a full shared budget: every
    /// live query holding memory is waiting for some to be released, so none
    /// will be, and this is the youngest of them.
    pub(super) fn must_yield(&self) -> bool {
        registry().must_yield(&self.state)
    }
}

#[derive(Default)]
struct Registry(Mutex<Vec<Weak<QueryState>>>);

impl Registry {
    fn register(&self) -> ExecutionCancellation {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let state = Arc::new(QueryState {
            sequence: SEQUENCE.fetch_add(1, Ordering::Relaxed),
            ..QueryState::default()
        });
        let mut entries = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|entry| entry.strong_count() > 0);
        entries.push(Arc::downgrade(&state));
        ExecutionCancellation { state }
    }

    /// The standstill test behind [`ExecutionCancellation::must_yield`]. A
    /// query holding nothing never yields - refusing it frees nothing - and
    /// with no holder at all the budget is held by nothing that will release
    /// it, so every waiter yields.
    fn must_yield(&self, state: &Arc<QueryState>) -> bool {
        let entries = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut youngest: Option<u64> = None;
        for holder in entries.iter().filter_map(Weak::upgrade) {
            if holder.cancelled.load(Ordering::Acquire) || holder.bytes.load(Ordering::Relaxed) == 0
            {
                continue;
            }
            if !holder.waiting.load(Ordering::Acquire) {
                return false;
            }
            youngest = youngest.max(Some(holder.sequence));
        }
        youngest.is_none_or(|youngest| youngest == state.sequence)
    }

    fn cancel_under_pressure(&self, used: usize, limit: usize) -> Option<usize> {
        if limit == 0 || used < limit.saturating_sub(limit / 10) {
            return None;
        }
        let mut entries = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|entry| entry.strong_count() > 0);
        let victim = entries
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|state| !state.cancelled.load(Ordering::Acquire))
            .map(|state| {
                let bytes = state.bytes.load(Ordering::Relaxed);
                (state, bytes)
            })
            .filter(|(_, bytes)| *bytes > 0)
            .max_by_key(|(_, bytes)| *bytes)?;
        victim.0.cancelled.store(true, Ordering::Release);
        Some(victim.1)
    }
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

/// At 90% of a nonzero memory limit, cancel at most one live query: the
/// largest by current tracked operator reservations. Already-cancelled
/// queries are skipped. Sampling and cadence are owned by the server.
/// Returns the victim's tracked bytes, for diagnostics.
#[must_use]
pub fn cancel_query_under_memory_pressure(used: usize, limit: usize) -> Option<usize> {
    registry().cancel_under_pressure(used, limit)
}

#[cfg(test)]
mod tests {
    use super::Registry;
    use crate::{ExecError, MemoryTracker, with_execution_cancellation};

    #[test]
    fn pressure_cancels_the_largest_query_and_releases_its_reservations() {
        let _serial = super::super::budget_serial::Serial::acquire();
        let registry = Registry::default();
        let small = registry.register();
        let large = registry.register();
        let a = with_execution_cancellation(small.clone(), || MemoryTracker::new(1000));
        let b = with_execution_cancellation(large.clone(), || MemoryTracker::new(1000));
        a.reserve(100).expect("small fits");
        b.reserve(300).expect("large fits");
        assert_eq!(registry.cancel_under_pressure(899, 1000), None);
        assert_eq!(registry.cancel_under_pressure(1000, 0), None);
        assert_eq!(registry.cancel_under_pressure(900, 1000), Some(300));
        assert!(!small.is_cancelled());
        assert!(matches!(b.reserve(1), Err(ExecError::QueryCancelled)));
        drop(b);
        assert_eq!(
            large.state.bytes.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(registry.cancel_under_pressure(950, 1000), Some(100));
        assert_eq!(registry.cancel_under_pressure(950, 1000), None);
    }

    #[test]
    fn only_the_youngest_holder_yields_and_only_when_every_holder_waits() {
        let registry = Registry::default();
        let older = registry.register();
        let younger = registry.register();
        let idle = registry.register();
        older.reserve(100);
        younger.reserve(50);
        idle.set_waiting(true);
        younger.set_waiting(true);
        // The older query still runs and will release: nobody yields.
        assert!(!registry.must_yield(&younger.state));
        older.set_waiting(true);
        // A standstill: the youngest holder gives way, the older waits on,
        // and a query holding nothing never yields (refusing it frees nothing).
        assert!(registry.must_yield(&younger.state));
        assert!(!registry.must_yield(&older.state));
        assert!(!registry.must_yield(&idle.state));
        // Once the younger is gone, the older is the youngest holder left.
        younger.cancel();
        assert!(registry.must_yield(&older.state));
    }

    #[test]
    fn clones_charge_only_new_reservations_and_finished_queries_leave_no_victim() {
        let _serial = super::super::budget_serial::Serial::acquire();
        let registry = Registry::default();
        let query = registry.register();
        let parent = with_execution_cancellation(query.clone(), || MemoryTracker::new(1000));
        parent.reserve(100).expect("parent");
        let clone = parent.clone();
        clone.reserve(40).expect("clone");
        let worker = parent.unbounded_worker();
        worker.reserve(900).expect("already accounted by parent");
        assert_eq!(
            query.state.bytes.load(std::sync::atomic::Ordering::Relaxed),
            140
        );
        clone.release(1000);
        assert_eq!(
            query.state.bytes.load(std::sync::atomic::Ordering::Relaxed),
            100
        );
        drop((parent, clone, worker));
        assert_eq!(registry.cancel_under_pressure(1000, 1000), None);
        drop(query);
        assert_eq!(registry.cancel_under_pressure(1000, 1000), None);
        assert!(registry.0.lock().expect("registry").is_empty());
    }
}
