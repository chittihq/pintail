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
//!
//! A full budget is contention, not a verdict on the query that found it
//! full. Refusing on the spot turned a burst of identical reports into a
//! burst of failures, each arriving query tripping over memory the others
//! were about to give back. [`MemoryBudget::reserve_patiently`] waits for a
//! release instead, and hands the error back only when waiting cannot help:
//! the request is larger than the whole budget, the caller is interrupted,
//! patience runs out, or every query holding memory is itself waiting - a
//! standstill only a refusal breaks.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

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
    limit: AtomicUsize,
    used: AtomicUsize,
    /// Reservations currently waiting for a release; a release takes the
    /// lock and wakes them only when there are any.
    waiters: AtomicUsize,
    waiting: Mutex<()>,
    released: Condvar,
    /// Asked for bytes when a reservation finds the budget full: memory
    /// held on sufferance (cached blocks) is given back through it before
    /// a query is refused or made to wait.
    reclaim: std::sync::OnceLock<fn(usize) -> usize>,
}

/// How long one wait for a release lasts before the waiter looks again at
/// its interruption and at whether it is the one that must yield.
const WAIT_SLICE: Duration = Duration::from_millis(20);

impl MemoryBudget {
    /// A budget of `limit` bytes. Zero is unbounded, so an operator can
    /// return to the previous behaviour deliberately rather than by
    /// accidentally configuring a budget nothing fits in.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self {
            limit: AtomicUsize::new(limit),
            used: AtomicUsize::new(0),
            waiters: AtomicUsize::new(0),
            waiting: Mutex::new(()),
            released: Condvar::new(),
            reclaim: std::sync::OnceLock::new(),
        }
    }

    /// Names what gives memory back when the budget is full. It is handed
    /// the bytes a refused reservation asked for and answers how many it
    /// released through [`Self::release`]. Set once; later calls are
    /// ignored.
    pub fn set_reclaim(&self, reclaim: fn(usize) -> usize) {
        let _ = self.reclaim.set(reclaim);
    }

    /// Sets the ceiling on an existing budget.
    ///
    /// The process-wide budget lives behind a `OnceLock`, and anything that
    /// merely READS it before startup configures it would win that race and
    /// pin the limit at zero - unbounded - for the life of the process. A
    /// limit that silently stops limiting is worse than no limit, because
    /// nothing says it happened. Setting the ceiling in place makes
    /// configuration win regardless of who looked first.
    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    /// The configured ceiling; zero means unbounded.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
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
        match self.reserve_as_is(bytes) {
            Ok(()) => Ok(()),
            Err(refused) => match self.reclaim.get() {
                Some(reclaim) if reclaim(bytes) > 0 => self.reserve_as_is(bytes),
                _ => Err(refused),
            },
        }
    }

    /// [`Self::reserve`] without asking anything to give memory back: what
    /// the holder of reclaimable memory itself reserves through, so that a
    /// full budget does not have it evict to make room for itself.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::MemoryLimitExceeded`] with server scope when the
    /// process budget cannot cover the request.
    pub fn reserve_as_is(&self, bytes: usize) -> Result<(), ExecError> {
        if self.limit() == 0 {
            return Ok(());
        }
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                let requested = used.saturating_add(bytes);
                (requested <= self.limit()).then_some(requested)
            })
            .map(|_| ())
            .map_err(|used| ExecError::MemoryLimitExceeded {
                used,
                requested: bytes,
                limit: self.limit(),
                scope: MemoryScope::Server,
            })
    }

    /// Returns `bytes` to the shared budget.
    pub fn release(&self, bytes: usize) {
        if self.limit() == 0 {
            return;
        }
        let _ = self
            .used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                Some(used.saturating_sub(bytes))
            });
        if self.waiters.load(Ordering::Acquire) > 0 {
            let _held = self
                .waiting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.released.notify_all();
        }
    }

    /// Takes `bytes`, waiting for other reservations to be released when the
    /// budget is full.
    ///
    /// `interrupted` is consulted between waits and ends the wait with its
    /// error. `must_yield` says the caller is the one that has to give way -
    /// every holder is waiting and nothing will be released - and ends the
    /// wait with the budget's own refusal, which a spilling operator answers
    /// by going to disk and anything else by failing and releasing what it
    /// held. `patience` bounds the whole wait.
    ///
    /// # Errors
    ///
    /// [`ExecError::MemoryLimitExceeded`] with server scope when the request
    /// can never fit, when the caller must yield or when patience runs out;
    /// whatever `interrupted` returns when it interrupts.
    pub fn reserve_patiently(
        &self,
        bytes: usize,
        patience: Duration,
        interrupted: impl Fn() -> Result<(), ExecError>,
        must_yield: impl Fn() -> bool,
    ) -> Result<(), ExecError> {
        let refused = match self.reserve(bytes) {
            Ok(()) => return Ok(()),
            Err(refused) => refused,
        };
        if bytes > self.limit() {
            return Err(refused);
        }
        let started = Instant::now();
        self.waiters.fetch_add(1, Ordering::AcqRel);
        let outcome = loop {
            if let Err(interruption) = interrupted() {
                break Err(interruption);
            }
            match self.reserve(bytes) {
                Ok(()) => break Ok(()),
                Err(refused) if started.elapsed() >= patience || must_yield() => {
                    break Err(refused);
                }
                Err(_) => {}
            }
            let held = self
                .waiting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A release between the refusal above and this wait wakes no
            // one; the slice bounds how long that can go unnoticed.
            let _ = self.released.wait_timeout(held, WAIT_SLICE);
        };
        self.waiters.fetch_sub(1, Ordering::AcqRel);
        outcome
    }

    /// Whether `bytes` would fit without taking them.
    #[must_use]
    pub fn would_fit(&self, bytes: usize) -> bool {
        self.limit() == 0 || self.used().saturating_add(bytes) <= self.limit()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{MemoryBudget, MemoryScope};
    use crate::ExecError;

    const PATIENT: Duration = Duration::from_secs(30);

    #[test]
    fn a_full_budget_waits_for_a_release_instead_of_refusing() {
        let budget = Arc::new(MemoryBudget::new(100));
        budget.reserve(80).expect("the first query fits");
        let releaser = {
            let budget = Arc::clone(&budget);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                budget.release(80);
            })
        };
        budget
            .reserve_patiently(40, PATIENT, || Ok(()), || false)
            .expect("the release makes room");
        releaser.join().expect("releaser");
        assert_eq!(budget.used(), 40);
    }

    #[test]
    fn waiting_ends_with_the_refusal_when_waiting_cannot_help() {
        let budget = MemoryBudget::new(100);
        budget.reserve(80).expect("fits");
        let started = Instant::now();
        // Larger than the whole budget: no release could ever make room.
        assert!(matches!(
            budget.reserve_patiently(101, PATIENT, || Ok(()), || false),
            Err(ExecError::MemoryLimitExceeded {
                scope: MemoryScope::Server,
                ..
            })
        ));
        // Every holder is waiting: this caller is the one to give way.
        assert!(matches!(
            budget.reserve_patiently(40, PATIENT, || Ok(()), || true),
            Err(ExecError::MemoryLimitExceeded { .. })
        ));
        // Interrupted while waiting: the interruption, not the refusal.
        assert_eq!(
            budget.reserve_patiently(40, PATIENT, || Err(ExecError::QueryCancelled), || false),
            Err(ExecError::QueryCancelled)
        );
        // Patience spent.
        assert!(
            budget
                .reserve_patiently(40, Duration::from_millis(30), || Ok(()), || false)
                .is_err()
        );
        assert!(
            started.elapsed() < PATIENT,
            "no refusal waited out its patience"
        );
        assert_eq!(budget.used(), 80, "no refused wait was charged");
    }

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
