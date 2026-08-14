//! Prototype e44: test whether a proof-gated fine payload decoder helps Top-K.

use std::collections::BTreeSet;

use common::{Lcg, bench};

const ROWS: usize = 65_536;
const K: usize = 100;
const LATE_PAGE: usize = 128;
const FOVEAL_PAGE: usize = 16;

#[derive(Clone, Copy)]
enum Shape {
    WideClustered,
    WideScattered,
    NarrowClustered,
    Uncorrelated,
}

#[derive(Clone, Copy)]
enum Policy {
    Eager,
    Late,
    Foveated,
}

struct Fixture {
    scores: Vec<u32>,
    bounds: Vec<u16>,
    payload: Vec<[u64; 8]>,
    payload_words: usize,
}

struct Outcome {
    answer: u64,
    work_digest: u64,
    payload_bytes: u64,
    fine_decoder_used: bool,
}

fn main() {
    println!("e44 — foveated Top-K materialization (executable prototype, audited)");
    for shape in [
        Shape::WideClustered,
        Shape::WideScattered,
        Shape::NarrowClustered,
        Shape::Uncorrelated,
    ] {
        let fixture = fixture(shape);
        let policies = [
            ("eager rows", Policy::Eager),
            ("score-only late", Policy::Late),
            ("proof-gated foveation", Policy::Foveated),
        ];
        let outcomes = policies.map(|(_, policy)| execute(&fixture, policy));
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.answer == outcomes[0].answer)
        );
        println!("\n=== {} ===", shape_name(shape));
        for ((name, _), outcome) in policies.iter().zip(outcomes.iter()) {
            println!(
                "{name:<24} payload bytes {:>9} fine decoder {} answer {:016x}",
                outcome.payload_bytes, outcome.fine_decoder_used, outcome.answer,
            );
        }
        for (name, policy) in policies {
            let _ = bench(name, || execute(&fixture, policy).work_digest);
        }
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::WideClustered => "wide payload, clustered leaders",
        Shape::WideScattered => "wide payload, scattered leaders",
        Shape::NarrowClustered => "narrow payload, clustered leaders",
        Shape::Uncorrelated => "uncorrelated proof bounds",
    }
}

fn fixture(shape: Shape) -> Fixture {
    let mut random = Lcg::new(0x4400_0044 ^ shape as u64);
    let mut scores = (0..ROWS)
        .map(|_| random.below(1_000_000) as u32)
        .collect::<Vec<_>>();
    match shape {
        Shape::WideClustered | Shape::NarrowClustered | Shape::Uncorrelated => {
            for (offset, score) in scores.iter_mut().skip(20_000).take(K).enumerate() {
                *score = 2_000_000 + offset as u32;
            }
        }
        Shape::WideScattered => {
            for rank in 0..K {
                scores[rank * 641 % ROWS] = 2_000_000 + rank as u32;
            }
        }
    }
    let bounds = scores
        .iter()
        .map(|score| {
            if matches!(shape, Shape::Uncorrelated) {
                random.below(u16::MAX as u64) as u16
            } else {
                (score >> 5).min(u32::from(u16::MAX)) as u16
            }
        })
        .collect();
    let payload = (0..ROWS)
        .map(|row| std::array::from_fn(|word| random.next_u64() ^ (row + word) as u64))
        .collect();
    Fixture {
        scores,
        bounds,
        payload,
        payload_words: if matches!(shape, Shape::NarrowClustered) {
            1
        } else {
            8
        },
    }
}

fn top_k(scores: &[u32]) -> Vec<usize> {
    let mut rows = (0..scores.len()).collect::<Vec<_>>();
    rows.select_nth_unstable_by(K, |left, right| {
        scores[*right]
            .cmp(&scores[*left])
            .then_with(|| left.cmp(right))
    });
    rows.truncate(K);
    rows.sort_unstable_by(|left, right| {
        scores[*right]
            .cmp(&scores[*left])
            .then_with(|| left.cmp(right))
    });
    rows
}

fn proof_is_correlated(fixture: &Fixture, winners: &[usize]) -> bool {
    let threshold = fixture
        .bounds
        .iter()
        .copied()
        .max()
        .unwrap_or_default()
        .saturating_sub(64);
    winners
        .iter()
        .filter(|row| fixture.bounds[**row] >= threshold)
        .count()
        >= K * 9 / 10
}

fn execute(fixture: &Fixture, policy: Policy) -> Outcome {
    let winners = top_k(&fixture.scores);
    let fine_decoder_used =
        matches!(policy, Policy::Foveated) && proof_is_correlated(fixture, &winners);
    let page_rows = match policy {
        Policy::Eager => ROWS,
        Policy::Late => LATE_PAGE,
        Policy::Foveated if fine_decoder_used => FOVEAL_PAGE,
        Policy::Foveated => LATE_PAGE,
    };
    let pages = if matches!(policy, Policy::Eager) {
        BTreeSet::from([0])
    } else {
        winners.iter().map(|row| row / page_rows).collect()
    };

    let mut work_digest = 0xcbf2_9ce4_8422_2325_u64;
    for page in pages {
        let start = page * page_rows;
        let end = (start + page_rows).min(ROWS);
        for payload in &fixture.payload[start..end] {
            for value in &payload[..fixture.payload_words] {
                work_digest = work_digest.rotate_left(7) ^ *value;
            }
        }
    }
    let answer = winners.iter().fold(0_u64, |checksum, row| {
        let payload = fixture.payload[*row][..fixture.payload_words]
            .iter()
            .fold(0_u64, |hash, value| hash.rotate_left(3) ^ *value);
        checksum
            .rotate_left(11)
            .wrapping_add(u64::from(fixture.scores[*row]) ^ payload)
    });
    let decoded_rows = if matches!(policy, Policy::Eager) {
        ROWS
    } else {
        winners
            .iter()
            .map(|row| row / page_rows)
            .collect::<BTreeSet<_>>()
            .len()
            * page_rows
    };
    Outcome {
        answer,
        work_digest: work_digest ^ answer,
        payload_bytes: (decoded_rows * fixture.payload_words * 8) as u64,
        fine_decoder_used,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_materializers_return_the_exact_same_ordered_top_k() {
        for shape in [
            Shape::WideClustered,
            Shape::WideScattered,
            Shape::NarrowClustered,
            Shape::Uncorrelated,
        ] {
            let fixture = fixture(shape);
            let eager = execute(&fixture, Policy::Eager).answer;
            assert_eq!(execute(&fixture, Policy::Late).answer, eager);
            assert_eq!(execute(&fixture, Policy::Foveated).answer, eager);
        }
    }

    #[test]
    fn uncorrelated_bounds_force_the_safe_late_materialization_fallback() {
        let fixture = fixture(Shape::Uncorrelated);
        let late = execute(&fixture, Policy::Late);
        let foveated = execute(&fixture, Policy::Foveated);
        assert!(!foveated.fine_decoder_used);
        assert_eq!(foveated.payload_bytes, late.payload_bytes);
    }

    #[test]
    fn correlated_bounds_activate_fine_payload_pages() {
        let fixture = fixture(Shape::WideScattered);
        let late = execute(&fixture, Policy::Late);
        let foveated = execute(&fixture, Policy::Foveated);
        assert!(foveated.fine_decoder_used);
        assert!(foveated.payload_bytes * 2 < late.payload_bytes);
    }
}
