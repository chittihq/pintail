//! Threads for the two parts of a bulk write that are independent by
//! construction: converting the rows of a copied chunk, each from its own
//! source row, and encoding the columns of a segment, each into its own
//! bytes.
//!
//! A table copy converted and encoded on the one thread that also waited
//! for the source, so a large table was copied at the speed of a single
//! core however many the machine had. The work is spread here, over a pool
//! of its own: the scan pool belongs to the statements being answered, and
//! a copy that took its threads would be answered for in query latency.
//!
//! The pool is half the machine's threads, at most eight;
//! `PINTAIL_INGEST_THREADS` sets it, and one thread - also what a machine
//! of two or three gets - keeps the work on the calling thread exactly as
//! before. Results are assembled in input order, so a segment's bytes do
//! not depend on the width.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

/// Fewer items than this are not worth handing to other threads.
const SPREAD_FROM: usize = 2;

static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
static SPREAD_BATCHES: AtomicU64 = AtomicU64::new(0);

/// Threads a bulk write spreads over: `PINTAIL_INGEST_THREADS` when set,
/// else half the machine's, at most eight. One means the caller's own.
#[must_use]
pub fn ingest_threads() -> usize {
    std::env::var("PINTAIL_INGEST_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|threads| *threads > 0)
        .unwrap_or_else(|| {
            let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
            (threads / 2).clamp(1, 8)
        })
}

fn pool() -> Option<&'static rayon::ThreadPool> {
    POOL.get_or_init(|| {
        let threads = ingest_threads();
        if threads < 2 {
            return None;
        }
        // A pool that cannot be built leaves the work where it was.
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("pintail-ingest-{index}"))
            .build()
            .ok()
    })
    .as_ref()
}

/// Batches spread over the pool since the process started: the evidence
/// that a bulk write took the wide path.
#[must_use]
pub fn ingest_batches_spread() -> u64 {
    SPREAD_BATCHES.load(Ordering::Relaxed)
}

/// Applies `work` to every item with its index, across the pool when it
/// has one and there is more than one item, and returns the results in
/// input order, or the first error in that order.
///
/// # Errors
///
/// Returns the error of the first item, in input order, whose work failed.
pub fn ingest_map<T, R, E>(
    items: Vec<T>,
    work: impl Fn(usize, T) -> Result<R, E> + Sync,
) -> Result<Vec<R>, E>
where
    T: Send,
    R: Send,
    E: Send,
{
    match pool() {
        Some(pool) if items.len() >= SPREAD_FROM => {
            SPREAD_BATCHES.fetch_add(1, Ordering::Relaxed);
            pool.install(|| {
                items
                    .into_par_iter()
                    .enumerate()
                    .map(|(index, item)| work(index, item))
                    .collect::<Vec<_>>()
            })
            .into_iter()
            .collect()
        }
        _ => items
            .into_iter()
            .enumerate()
            .map(|(index, item)| work(index, item))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ingest_batches_spread, ingest_map, pool};

    #[test]
    fn results_keep_input_order_and_the_first_error_wins() {
        let doubled = ingest_map((0..10_000_u64).collect(), |index, value| {
            assert_eq!(index as u64, value);
            Ok::<_, String>(value * 2)
        })
        .unwrap();
        assert_eq!(
            doubled,
            (0..10_000_u64).map(|value| value * 2).collect::<Vec<_>>()
        );
        let failed = ingest_map((0..10_000_u64).collect(), |_, value| {
            if value % 1_000 == 999 {
                Err(value)
            } else {
                Ok(value)
            }
        });
        assert_eq!(failed, Err(999), "the first failure in input order");
    }

    #[test]
    fn a_batch_is_counted_when_it_is_spread() {
        // Other tests of the process may spread batches of their own.
        let before = ingest_batches_spread();
        ingest_map(vec![1_u8, 2], |_, value| Ok::<_, ()>(value)).unwrap();
        assert!(
            ingest_batches_spread() - before >= u64::from(pool().is_some()),
            "a batch of two is spread when there is a pool"
        );
        if pool().is_none() {
            assert_eq!(ingest_batches_spread(), 0, "nothing spreads without one");
        }
    }
}
