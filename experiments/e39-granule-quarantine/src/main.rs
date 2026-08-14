//! e39: persist verified corrupt immutable-granule intervals.
//!
//! Unlike the invalidated model, this harness stores bytes, corrupts them, verifies
//! per-granule checksums, executes queries, and compares every outcome with a pristine
//! segment oracle.

use common::{Lcg, bench};

const GRANULES: usize = 128;
const BYTES_PER_GRANULE: usize = 512;
const QUERIES: usize = 4_000;
const CORRUPT: [usize; 3] = [20, 63, 105];

#[derive(Clone, Copy)]
enum Policy {
    WholeSegmentRediscovery,
    PersistedQuarantine,
}

#[derive(Clone, Copy)]
struct Query {
    start: usize,
    end: usize,
    token: u64,
}

struct Segment {
    pristine: Vec<Vec<u8>>,
    damaged: Vec<Vec<u8>>,
    checksums: Vec<u64>,
}

#[derive(Default)]
struct Outcome {
    outcome_checksum: u64,
    verified_bytes: u64,
    corrupt_granules_found: usize,
    overlap_queries: u64,
    silent_or_wrong: u64,
    disjoint_queries: u64,
    successful_disjoint: u64,
}

fn main() {
    println!("e39 — granule quarantine membranes (byte-path kernel, audited)");
    let segment = segment();
    let queries = queries();
    for (name, policy) in [
        ("whole-segment rediscovery", Policy::WholeSegmentRediscovery),
        ("persisted quarantine", Policy::PersistedQuarantine),
    ] {
        let outcome = execute(&segment, &queries, policy);
        println!(
            "{name:<27} verify {:>10} B found {}/{} silent/wrong {} availability {:>6.2}%",
            outcome.verified_bytes,
            outcome.corrupt_granules_found,
            CORRUPT.len(),
            outcome.silent_or_wrong,
            percentage(outcome.successful_disjoint, outcome.disjoint_queries),
        );
    }

    bench("whole-segment rediscovery", || {
        execute(&segment, &queries, Policy::WholeSegmentRediscovery).outcome_checksum
    });
    bench("persisted quarantine", || {
        execute(&segment, &queries, Policy::PersistedQuarantine).outcome_checksum
    });
}

fn percentage(numerator: u64, denominator: u64) -> f64 {
    numerator as f64 * 100.0 / denominator.max(1) as f64
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}

fn segment() -> Segment {
    let mut random = Lcg::new(0x3900_0039);
    let pristine = (0..GRANULES)
        .map(|_| {
            (0..BYTES_PER_GRANULE)
                .map(|_| random.below(256) as u8)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let checksums = pristine.iter().map(|bytes| checksum(bytes)).collect();
    let mut damaged = pristine.clone();
    for (offset, granule) in CORRUPT.into_iter().enumerate() {
        damaged[granule][17 + offset] ^= 0x5a;
    }
    Segment {
        pristine,
        damaged,
        checksums,
    }
}

fn queries() -> Vec<Query> {
    let mut random = Lcg::new(0x3900_1039);
    (0..QUERIES)
        .map(|index| {
            let width = 1 + random.below(8) as usize;
            let start = if index % 11 == 0 {
                CORRUPT[(index / 11) % CORRUPT.len()].saturating_sub(width / 2)
            } else {
                random.below((GRANULES - width) as u64) as usize
            };
            Query {
                start,
                end: start + width,
                token: random.next_u64(),
            }
        })
        .collect()
}

fn discover(segment: &Segment, verified_bytes: &mut u64) -> Vec<bool> {
    segment
        .damaged
        .iter()
        .zip(&segment.checksums)
        .map(|(bytes, expected)| {
            *verified_bytes += bytes.len() as u64;
            checksum(bytes) != *expected
        })
        .collect()
}

fn oracle(segment: &Segment, query: Query) -> u64 {
    (query.start..query.end)
        .map(|granule| checksum(&segment.pristine[granule]))
        .fold(0, |acc, value| acc.rotate_left(9) ^ value)
}

fn read_range(segment: &Segment, query: Query, verified_bytes: &mut u64) -> Result<u64, ()> {
    let mut answer = 0_u64;
    for granule in query.start..query.end {
        let bytes = &segment.damaged[granule];
        *verified_bytes += bytes.len() as u64;
        let actual = checksum(bytes);
        if actual != segment.checksums[granule] {
            return Err(());
        }
        answer = answer.rotate_left(9) ^ actual;
    }
    Ok(answer)
}

fn execute(segment: &Segment, queries: &[Query], policy: Policy) -> Outcome {
    let mut outcome = Outcome::default();
    let persisted = if matches!(policy, Policy::PersistedQuarantine) {
        discover(segment, &mut outcome.verified_bytes)
    } else {
        vec![false; GRANULES]
    };
    outcome.corrupt_granules_found = persisted.iter().filter(|bad| **bad).count();

    for &query in queries {
        let overlaps_corruption = CORRUPT
            .into_iter()
            .any(|granule| query.start <= granule && granule < query.end);
        outcome.overlap_queries += u64::from(overlaps_corruption);
        outcome.disjoint_queries += u64::from(!overlaps_corruption);

        let result = match policy {
            Policy::WholeSegmentRediscovery => {
                let bad = discover(segment, &mut outcome.verified_bytes);
                outcome.corrupt_granules_found = bad.iter().filter(|value| **value).count();
                if bad.into_iter().any(|value| value) {
                    Err(())
                } else {
                    read_range(segment, query, &mut outcome.verified_bytes)
                }
            }
            Policy::PersistedQuarantine => {
                if persisted[query.start..query.end].iter().any(|bad| *bad) {
                    Err(())
                } else {
                    read_range(segment, query, &mut outcome.verified_bytes)
                }
            }
        };

        match result {
            Ok(answer) => {
                outcome.successful_disjoint += u64::from(!overlaps_corruption);
                outcome.silent_or_wrong +=
                    u64::from(overlaps_corruption || answer != oracle(segment, query));
                outcome.outcome_checksum =
                    (outcome.outcome_checksum ^ answer ^ query.token).wrapping_mul(0x100_0000_01b3);
            }
            Err(()) => {
                outcome.outcome_checksum =
                    (outcome.outcome_checksum ^ 0xdead_beef_dead_beef ^ query.token)
                        .wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corruption_is_injected_into_real_bytes_and_discovered() {
        let segment = segment();
        let mut verified = 0;
        let bad = discover(&segment, &mut verified);
        assert_eq!(bad.iter().filter(|value| **value).count(), CORRUPT.len());
        assert!(CORRUPT.into_iter().all(|granule| bad[granule]));
        assert_eq!(verified, (GRANULES * BYTES_PER_GRANULE) as u64);
    }

    #[test]
    fn quarantine_answers_disjoint_ranges_and_fails_overlap() {
        let segment = segment();
        let queries = queries();
        let outcome = execute(&segment, &queries, Policy::PersistedQuarantine);
        assert_eq!(outcome.silent_or_wrong, 0);
        assert_eq!(outcome.successful_disjoint, outcome.disjoint_queries);
        assert!(outcome.overlap_queries > 0);
    }

    #[test]
    fn whole_segment_policy_really_loses_disjoint_availability() {
        let segment = segment();
        let outcome = execute(&segment, &queries(), Policy::WholeSegmentRediscovery);
        assert_eq!(outcome.successful_disjoint, 0);
        assert!(outcome.disjoint_queries > 0);
    }
}
