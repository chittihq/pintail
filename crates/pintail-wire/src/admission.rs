//! Bounds how many queries execute at once.
//!
//! Without this the server admits every query it is handed. The load
//! baseline (`tests/load/results.md`) measured what that costs: p99 tracks
//! concurrency almost exactly — 4x the clients gave 4.0x the latency, 2x
//! gave 2.2x — reaching 22 seconds at 256 concurrent clients with zero
//! failures. Nothing was refused, so the server kept spending capacity on
//! answers whose clients had long since timed out.
//!
//! A bound converts that into backpressure. Queries beyond the limit wait
//! briefly for a slot; if none frees, they are refused immediately with a
//! clear error rather than silently queued behind twenty seconds of work.
//! Fast refusal is the useful behaviour: a client can retry or shed, which
//! it cannot do while blocked.
//!
//! The permit covers execution only. Connections are still accepted freely,
//! so idle sessions and metadata lookups are unaffected by a busy engine.

use std::{
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::Duration,
};

/// The process-wide bound.
///
/// It has to be process-wide rather than per engine: the HTTP surface
/// builds a fresh `ReplicaEngine` for every request, so an instance-owned
/// counter would hand each request a full, private allowance and bound
/// nothing. Cores and memory are properties of the process, so the limit
/// is too.
static SHARED: OnceLock<Arc<QueryAdmission>> = OnceLock::new();

/// Installs the process-wide bound. Called once at startup; later calls are
/// ignored so a stray caller cannot loosen a configured limit.
pub fn init_shared_admission(limit: usize) {
    let _ = SHARED.set(Arc::new(QueryAdmission::new(limit)));
}

/// The process-wide bound, defaulting if startup never configured one.
#[must_use]
pub fn shared_admission() -> Arc<QueryAdmission> {
    Arc::clone(
        SHARED.get_or_init(|| Arc::new(QueryAdmission::new(default_max_concurrent_queries()))),
    )
}

/// How long a query waits for a slot before it is refused. Long enough to
/// absorb a burst that clears quickly, short enough that the caller learns
/// the server is saturated while its own deadline still has room.
const DEFAULT_QUEUE_WAIT: Duration = Duration::from_secs(2);

/// Concurrency limit when the operator sets none. Past the point where
/// every core is busy, more concurrent queries buy no throughput and only
/// add latency, so the default is a small multiple of the core count rather
/// than an arbitrary large number.
#[must_use]
pub fn default_max_concurrent_queries() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    cores.saturating_mul(4).max(16)
}

/// A counting semaphore with a bounded wait.
///
/// Deliberately blocking rather than async: queries already run on
/// `spawn_blocking` threads, so an async permit would have to be acquired
/// on the reactor and carried across the boundary, widening the change for
/// no benefit.
#[derive(Debug)]
pub struct QueryAdmission {
    limit: usize,
    available: Mutex<usize>,
    released: Condvar,
    wait: Duration,
}

impl QueryAdmission {
    /// Admission bounded to `limit` concurrent queries. A zero limit is
    /// treated as unbounded so an operator cannot accidentally wedge the
    /// server shut with a misconfigured value.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            available: Mutex::new(limit),
            released: Condvar::new(),
            wait: DEFAULT_QUEUE_WAIT,
        }
    }

    /// Same, with an explicit queue wait. Tests use this to observe refusal
    /// without waiting out the production timeout.
    #[must_use]
    pub fn with_wait(limit: usize, wait: Duration) -> Self {
        Self {
            wait,
            ..Self::new(limit)
        }
    }

    /// The configured ceiling; zero means unbounded.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Takes a slot, waiting up to the queue timeout. `None` means the
    /// server is saturated and the caller should refuse the query.
    #[must_use]
    pub fn try_admit(&self) -> Option<QueryPermit<'_>> {
        if self.limit == 0 {
            return Some(QueryPermit { admission: None });
        }
        let Ok(available) = self.available.lock() else {
            // A poisoned lock means another query panicked while holding
            // the count. Admitting is the safe direction: refusing every
            // query afterwards would turn one panic into an outage.
            return Some(QueryPermit { admission: None });
        };
        let Ok((mut available, timeout)) =
            self.released
                .wait_timeout_while(available, self.wait, |available| *available == 0)
        else {
            return Some(QueryPermit { admission: None });
        };
        if timeout.timed_out() && *available == 0 {
            return None;
        }
        *available -= 1;
        Some(QueryPermit {
            admission: Some(self),
        })
    }
}

/// Releases its slot when dropped, including on panic or early return, so a
/// failing query cannot leak capacity.
#[derive(Debug)]
pub struct QueryPermit<'admission> {
    admission: Option<&'admission QueryAdmission>,
}

impl Drop for QueryPermit<'_> {
    fn drop(&mut self) {
        let Some(admission) = self.admission else {
            return;
        };
        if let Ok(mut available) = admission.available.lock() {
            *available += 1;
            admission.released.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{QueryAdmission, default_max_concurrent_queries};
    use std::{sync::Arc, thread, time::Duration};

    #[test]
    fn a_permit_is_returned_when_capacity_exists() {
        let admission = QueryAdmission::new(2);
        let first = admission.try_admit();
        let second = admission.try_admit();
        assert!(first.is_some());
        assert!(second.is_some());
    }

    #[test]
    fn saturation_refuses_rather_than_queueing_forever() {
        // The whole point of the bound: past capacity the caller learns
        // immediately instead of waiting behind the queue.
        let admission = QueryAdmission::with_wait(1, Duration::from_millis(20));
        let held = admission.try_admit().expect("first admits");
        assert!(admission.try_admit().is_none(), "second must be refused");
        drop(held);
        assert!(admission.try_admit().is_some(), "slot returns on drop");
    }

    #[test]
    fn a_freed_slot_wakes_a_waiter_within_the_queue_wait() {
        // A burst that clears quickly must be absorbed, not refused: the
        // waiter is released as soon as the holder finishes.
        let admission = Arc::new(QueryAdmission::with_wait(1, Duration::from_secs(2)));
        let held = admission.try_admit().expect("first admits");
        let waiter = {
            let admission = Arc::clone(&admission);
            thread::spawn(move || admission.try_admit().is_some())
        };
        thread::sleep(Duration::from_millis(50));
        drop(held);
        assert!(
            waiter.join().expect("waiter thread"),
            "waiter must be admitted"
        );
    }

    #[test]
    fn a_dropped_permit_returns_capacity_even_after_a_panic() {
        let admission = Arc::new(QueryAdmission::with_wait(1, Duration::from_millis(20)));
        let panicking = {
            let admission = Arc::clone(&admission);
            thread::spawn(move || {
                let _permit = admission.try_admit().expect("admits");
                panic!("query panicked while holding a slot");
            })
        };
        assert!(panicking.join().is_err(), "thread must have panicked");
        assert!(
            admission.try_admit().is_some(),
            "a panicking query must not leak its slot"
        );
    }

    #[test]
    fn a_zero_limit_is_unbounded_rather_than_closed() {
        // Misconfiguring the limit to zero must not wedge the server shut.
        let admission = QueryAdmission::with_wait(0, Duration::from_millis(20));
        let first = admission.try_admit();
        let second = admission.try_admit();
        assert!(first.is_some());
        assert!(second.is_some());
    }

    #[test]
    fn the_default_limit_scales_with_cores_and_has_a_floor() {
        let limit = default_max_concurrent_queries();
        assert!(limit >= 16, "default must not starve a small machine");
    }
}
