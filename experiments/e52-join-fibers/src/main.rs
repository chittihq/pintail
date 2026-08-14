//! Prototype e52: test demand-grown per-segment join-edge fingerprints.

use std::collections::HashSet;

use common::{Lcg, bench};

const SEGMENTS: usize = 256;
const ROWS_PER_SEGMENT: usize = 256;
const REPEATS: u64 = 12;
const GROW_AFTER: u64 = 2;

#[derive(Clone, Copy)]
enum Shape {
    Star,
    Chain,
    Sparse,
    NoForeignKey,
}

#[derive(Clone, Copy)]
enum Policy {
    Raw,
    Bloom,
    StaticFiber,
    DemandFiber,
}

struct Fixture {
    segments: Vec<Vec<u32>>,
    target: HashSet<u32>,
    foreign_key: bool,
}

struct Bloom {
    words: [u64; 8],
}

struct Outcome {
    answer: u64,
    work_digest: u64,
    input_rows: u64,
    build_rows: u64,
    metadata_bytes: u64,
}

fn main() {
    println!("e52 — demand-grown join fibers (executable prototype, audited)");
    for shape in [
        Shape::Star,
        Shape::Chain,
        Shape::Sparse,
        Shape::NoForeignKey,
    ] {
        let fixture = fixture(shape);
        let blooms = build_blooms(&fixture);
        let fiber = build_fiber(&fixture);
        let policies = [
            ("raw hash join", Policy::Raw),
            ("ordinary Bloom", Policy::Bloom),
            ("static fingerprint", Policy::StaticFiber),
            ("demand-grown fiber", Policy::DemandFiber),
        ];
        let outcomes = policies.map(|(_, policy)| execute(&fixture, &blooms, &fiber, policy));
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.answer == outcomes[0].answer)
        );
        println!("\n=== {} ===", shape_name(shape));
        for ((name, _), outcome) in policies.iter().zip(outcomes.iter()) {
            println!(
                "{name:<23} input {:>8} build {:>7} metadata {:>4} B answer {:016x}",
                outcome.input_rows, outcome.build_rows, outcome.metadata_bytes, outcome.answer,
            );
        }
        for (name, policy) in policies {
            let _ = bench(name, || {
                execute(&fixture, &blooms, &fiber, policy).work_digest
            });
        }
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Star => "star join",
        Shape::Chain => "chain join",
        Shape::Sparse => "sparse match",
        Shape::NoForeignKey => "no declared FK",
    }
}

fn fixture(shape: Shape) -> Fixture {
    let mut random = Lcg::new(0x5200_0052 ^ shape as u64);
    let target = (0..16_u32).collect::<HashSet<_>>();
    let active_every = match shape {
        Shape::Star => 4,
        Shape::Chain => 3,
        Shape::Sparse => 16,
        Shape::NoForeignKey => 1,
    };
    let foreign_key = !matches!(shape, Shape::NoForeignKey);
    let segments = (0..SEGMENTS)
        .map(|segment| {
            (0..ROWS_PER_SEGMENT)
                .map(|row| {
                    if foreign_key && segment.is_multiple_of(active_every) && row < 24 {
                        random.below(16) as u32
                    } else {
                        1_000 + random.below(1_000_000) as u32
                    }
                })
                .collect()
        })
        .collect();
    Fixture {
        segments,
        target,
        foreign_key,
    }
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

impl Bloom {
    fn new() -> Self {
        Self { words: [0; 8] }
    }

    fn position(value: u32, hash: u64) -> usize {
        mix64(u64::from(value) ^ hash.wrapping_mul(0x9e37_79b9_7f4a_7c15)) as usize % 512
    }

    fn add(&mut self, value: u32) {
        for hash in 0..3 {
            let position = Self::position(value, hash);
            self.words[position / 64] |= 1_u64 << (position % 64);
        }
    }

    fn has(&self, value: u32) -> bool {
        (0..3).all(|hash| {
            let position = Self::position(value, hash);
            self.words[position / 64] & (1_u64 << (position % 64)) != 0
        })
    }
}

fn build_blooms(fixture: &Fixture) -> Vec<Bloom> {
    fixture
        .segments
        .iter()
        .map(|rows| {
            let mut bloom = Bloom::new();
            for &key in rows {
                bloom.add(key);
            }
            bloom
        })
        .collect()
}

fn build_fiber(fixture: &Fixture) -> Vec<bool> {
    build_fiber_measured(fixture).0
}

fn build_fiber_measured(fixture: &Fixture) -> (Vec<bool>, u64, u64) {
    let mut inspected = 0_u64;
    let mut digest = 0_u64;
    let fiber = fixture
        .segments
        .iter()
        .map(|rows| {
            for &key in rows {
                inspected += 1;
                digest = digest.rotate_left(3) ^ u64::from(key);
                if fixture.target.contains(&key) {
                    return true;
                }
            }
            false
        })
        .collect();
    (fiber, inspected, digest)
}

fn bloom_candidate(bloom: &Bloom, target: &HashSet<u32>) -> bool {
    target.iter().any(|key| bloom.has(*key))
}

fn scan_segment(rows: &[u32], target: &HashSet<u32>, digest: &mut u64) -> (u64, u64) {
    let mut matches = 0_u64;
    let mut sum = 0_u64;
    for &key in rows {
        *digest = digest.rotate_left(3) ^ u64::from(key);
        if target.contains(&key) {
            matches += 1;
            sum += u64::from(key);
        }
    }
    (matches, sum)
}

fn execute(fixture: &Fixture, blooms: &[Bloom], fiber: &[bool], policy: Policy) -> Outcome {
    let mut answer = 0xcbf2_9ce4_8422_2325_u64;
    let mut work_digest = 0_u64;
    let mut input_rows = 0_u64;
    let mut build_rows = 0_u64;
    let mut demand_fiber = None;

    for repeat in 0..REPEATS {
        let use_fiber = match policy {
            Policy::StaticFiber => fixture.foreign_key,
            Policy::DemandFiber => fixture.foreign_key && repeat >= GROW_AFTER,
            Policy::Raw | Policy::Bloom => false,
        };
        if matches!(policy, Policy::DemandFiber) && fixture.foreign_key && repeat == GROW_AFTER {
            let built = build_fiber_measured(fixture);
            demand_fiber = Some(built.0);
            build_rows += built.1;
            work_digest ^= built.2;
        }
        let mut matches = 0_u64;
        let mut sum = 0_u64;
        for segment in 0..SEGMENTS {
            let candidate = match policy {
                Policy::Raw => true,
                Policy::Bloom => {
                    !fixture.foreign_key || bloom_candidate(&blooms[segment], &fixture.target)
                }
                Policy::StaticFiber if use_fiber => fiber[segment],
                Policy::DemandFiber if use_fiber => demand_fiber
                    .as_ref()
                    .expect("fiber is built when demand threshold is reached")[segment],
                Policy::StaticFiber | Policy::DemandFiber => {
                    !fixture.foreign_key || bloom_candidate(&blooms[segment], &fixture.target)
                }
            };
            if candidate {
                input_rows += ROWS_PER_SEGMENT as u64;
                let result = scan_segment(
                    &fixture.segments[segment],
                    &fixture.target,
                    &mut work_digest,
                );
                matches += result.0;
                sum += result.1;
            }
        }
        answer = answer
            .rotate_left(7)
            .wrapping_add(matches.rotate_left(17) ^ sum);
    }
    let metadata_bytes = match policy {
        Policy::Raw => 0,
        Policy::Bloom => (SEGMENTS * 64) as u64,
        Policy::StaticFiber | Policy::DemandFiber if fixture.foreign_key => {
            (SEGMENTS / 8 + 8) as u64
        }
        Policy::StaticFiber | Policy::DemandFiber => (SEGMENTS * 64) as u64,
    };
    Outcome {
        answer,
        work_digest: work_digest ^ answer,
        input_rows,
        build_rows,
        metadata_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_policy_returns_the_exact_join_result() {
        for shape in [
            Shape::Star,
            Shape::Chain,
            Shape::Sparse,
            Shape::NoForeignKey,
        ] {
            let fixture = fixture(shape);
            let blooms = build_blooms(&fixture);
            let fiber = build_fiber(&fixture);
            let expected = execute(&fixture, &blooms, &fiber, Policy::Raw).answer;
            for policy in [Policy::Bloom, Policy::StaticFiber, Policy::DemandFiber] {
                assert_eq!(execute(&fixture, &blooms, &fiber, policy).answer, expected);
            }
        }
    }

    #[test]
    fn demand_fiber_is_neutral_without_a_declared_foreign_key() {
        let fixture = fixture(Shape::NoForeignKey);
        let blooms = build_blooms(&fixture);
        let fiber = build_fiber(&fixture);
        let bloom = execute(&fixture, &blooms, &fiber, Policy::Bloom);
        let demand = execute(&fixture, &blooms, &fiber, Policy::DemandFiber);
        assert_eq!(demand.input_rows, bloom.input_rows);
        assert_eq!(demand.build_rows, 0);
    }

    #[test]
    fn fiber_metadata_stays_below_two_percent_of_the_fk_column() {
        let fixture = fixture(Shape::Sparse);
        let blooms = build_blooms(&fixture);
        let fiber = build_fiber(&fixture);
        let outcome = execute(&fixture, &blooms, &fiber, Policy::DemandFiber);
        let column_bytes = (SEGMENTS * ROWS_PER_SEGMENT * size_of::<u32>()) as u64;
        assert!(outcome.metadata_bytes * 100 < column_bytes * 2);
    }
}
