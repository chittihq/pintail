//! Query lifetime and aggregate reservation accounting for pressure cancellation.
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Debug, Default)]
struct QueryState {
    cancelled: AtomicBool,
    bytes: AtomicUsize,
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

    pub(super) fn reserve(&self, bytes: usize) {
        self.state.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
    pub(super) fn release(&self, bytes: usize) {
        self.state.bytes.fetch_sub(bytes, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Registry(Mutex<Vec<Weak<QueryState>>>);

impl Registry {
    fn register(&self) -> ExecutionCancellation {
        let state = Arc::new(QueryState::default());
        let mut entries = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|entry| entry.strong_count() > 0);
        entries.push(Arc::downgrade(&state));
        ExecutionCancellation { state }
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
