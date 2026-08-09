//! A process-wide ceiling that every query's memory draws from.
//!
//! The per-query ceiling ([`super::MemoryTracker`]) bounds one query. It
//! says nothing about how many queries run at once, so the arithmetic bound
//! on the process is `concurrent_queries x per_query_limit` — a product
//! whose left factor the engine did not control until admission control
//! landed, and whose right factor it still does not sum.
//!
//! Admission control bounds the count, which is why resident memory stopped
//! growing with load (`tests/load/results.md`). It does not bound the total:
//! forty queries admitted under a four-gigabyte per-query ceiling can still
//! ask for a hundred and sixty gigabytes between them. This budget is the
//! missing term.
//!
//! Exhaustion is reported as [`ExecError::MemoryLimitExceeded`] rather than
//! a distinct variant, and that is deliberate. Spilling operators decide to
//! go to disk by matching that variant; a new one would compile fine and
//! silently stop them spilling at exactly the moment memory is scarcest.
//! The reported `scope` distinguishes the two ceilings for a reader without
//! changing what operators match on.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::ExecError;

/// Which ceiling a memory failure hit. Both are reported through the same
/// error so spill decisions keep working; this only tells them apart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryScope {
    /// One query's own ceiling.
    Query,
    /// The process-wide budget shared by every concurrent query.
    Server,
}

impl MemoryScope {
    /// Wording for the error message.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Server => "server",
        }
    }
}

/// A shared byte budget.
#[derive(Debug)]
pub struct MemoryBudget {
    limit: usize,
    used: AtomicUsize,
}

impl MemoryBudget {
    /// A budget of `limit` bytes. Zero is unbounded, so an operator can
    /// return to the previous behaviour deliberately rather than by
    /// accidentally configuring a budget nothing fits in.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }

    /// The configured ceiling; zero means unbounded.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes currently held across every query.
    #[must_use]
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// Takes `bytes` from the shared budget, or reports what was available.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::MemoryLimitExceeded`] with server scope when the
    /// process budget cannot cover the request.
    pub fn reserve(&self, bytes: usize) -> Result<(), ExecError> {
        if self.limit == 0 {
            return Ok(());
        }
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                let requested = used.saturating_add(bytes);
                (requested <= self.limit).then_some(requested)
            })
            .map(|_| ())
            .map_err(|used| ExecError::MemoryLimitExceeded {
                used,
                requested: bytes,
                limit: self.limit,
                scope: MemoryScope::Server,
            })
    }

    /// Returns `bytes` to the shared budget.
    pub fn release(&self, bytes: usize) {
        if self.limit == 0 {
            return;
        }
        let _ = self
            .used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                Some(used.saturating_sub(bytes))
            });
    }

    /// Whether `bytes` would fit without taking them.
    #[must_use]
    pub fn would_fit(&self, bytes: usize) -> bool {
        self.limit == 0 || self.used().saturating_add(bytes) <= self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryBudget, MemoryScope};
    use crate::ExecError;

    #[test]
    fn reservations_accumulate_and_release_symmetrically() {
        let budget = MemoryBudget::new(100);
        budget.reserve(60).expect("fits");
        assert_eq!(budget.used(), 60);
        budget.release(60);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn exhaustion_reports_server_scope_so_it_is_not_read_as_a_query_ceiling() {
        let budget = MemoryBudget::new(100);
        budget.reserve(80).expect("fits");
        let error = budget.reserve(40).expect_err("must not exceed the budget");
        match error {
            ExecError::MemoryLimitExceeded { scope, limit, .. } => {
                assert_eq!(scope, MemoryScope::Server);
                assert_eq!(limit, 100);
            }
            other => panic!("expected a memory limit error, got {other:?}"),
        }
        // The failed reservation must not have been charged.
        assert_eq!(budget.used(), 80);
    }

    #[test]
    fn a_zero_budget_is_unbounded_rather_than_closed() {
        let budget = MemoryBudget::new(0);
        budget.reserve(usize::MAX).expect("zero means unbounded");
        assert!(budget.would_fit(usize::MAX));
    }

    #[test]
    fn release_cannot_underflow_into_a_huge_allowance() {
        // Releasing more than was taken must clamp at zero; wrapping would
        // hand the next query an allowance of nearly usize::MAX.
        let budget = MemoryBudget::new(100);
        budget.reserve(10).expect("fits");
        budget.release(50);
        assert_eq!(budget.used(), 0);
        assert!(budget.reserve(100).is_ok());
    }

    #[test]
    fn the_budget_bounds_the_sum_that_per_query_ceilings_cannot() {
        // The defect this exists for: two queries each well inside their
        // own ceiling can still exceed what the process has. A per-query
        // limit cannot see that; a shared budget can.
        // Each query asks 60 bytes against its own 80-byte ceiling, so a
        // per-query limit admits both. The process has only 100.
        const PER_QUERY_REQUEST: usize = 60;
        let budget = MemoryBudget::new(100);
        budget.reserve(PER_QUERY_REQUEST).expect("first query fits");
        // ...but together they are not, and only the budget catches it.
        let error = budget
            .reserve(PER_QUERY_REQUEST)
            .expect_err("their sum must be refused");
        assert!(matches!(
            error,
            ExecError::MemoryLimitExceeded {
                scope: MemoryScope::Server,
                ..
            }
        ));
    }

    #[test]
    fn concurrent_reservations_never_oversubscribe_the_budget() {
        // The whole point of a shared budget: threads racing to reserve must
        // not sum past the ceiling.
        let budget = std::sync::Arc::new(MemoryBudget::new(1_000));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let budget = std::sync::Arc::clone(&budget);
                std::thread::spawn(move || {
                    let mut taken = 0;
                    for _ in 0..100 {
                        if budget.reserve(10).is_ok() {
                            taken += 10;
                        }
                    }
                    taken
                })
            })
            .collect();
        let taken: usize = threads.into_iter().map(|t| t.join().expect("thread")).sum();
        assert_eq!(taken, budget.used());
        assert!(
            budget.used() <= 1_000,
            "budget oversubscribed: {} > 1000",
            budget.used()
        );
    }
}
