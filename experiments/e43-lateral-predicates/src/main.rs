//! e43: order conjuncts by marginal (non-overlapping) rejection per unit work.
//!
//! Evidence tier: deterministic policy simulation over exact predicate truth
//! tables. `work` is calibrated predicate-cost units, not elapsed CPU time.

use std::hint::black_box;

use common::{Lcg, bench, check_consistency};

const ROWS: usize = 600_000;
const BLOCK: usize = 4096;
const SAMPLE: usize = 8;
const COST: [u64; 4] = [1, 4, 13, 3];

#[derive(Clone, Copy)]
enum Shape {
    Independent,
    Correlated,
    AntiCorrelated,
    Drift,
    FrontLoadedBias,
}

#[derive(Clone, Copy)]
enum Policy {
    Sql,
    StaticLearned,
    OracleStatic,
    Lateral,
}

struct Outcome {
    checksum: u64,
    matches: u64,
    work: u64,
    sampled_rows: u64,
}

fn main() {
    println!("e43 — lateral-inhibition predicate ordering (simulation tier)");
    println!("{ROWS} rows, four predicates, costs {COST:?}\n");
    for shape in [
        Shape::Independent,
        Shape::Correlated,
        Shape::AntiCorrelated,
        Shape::Drift,
        Shape::FrontLoadedBias,
    ] {
        let rows = generate(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("SQL order", Policy::Sql),
            ("learned once from first block", Policy::StaticLearned),
            ("hindsight best static", Policy::OracleStatic),
            ("per-block marginal inhibition", Policy::Lateral),
        ];
        let mut checksum = None;
        for (name, policy) in policies {
            let outcome = run(&rows, policy);
            if let Some(expected) = checksum {
                assert_eq!(expected, outcome.checksum);
            } else {
                checksum = Some(outcome.checksum);
            }
            println!(
                "{name:<33} work {:>11}  sampled {:>7}  matches {}",
                outcome.work, outcome.sampled_rows, outcome.matches
            );
        }
        let results = [
            bench("SQL", || observed(run(&rows, Policy::Sql))),
            bench("static learned", || {
                observed(run(&rows, Policy::StaticLearned))
            }),
            bench("oracle static", || {
                observed(run(&rows, Policy::OracleStatic))
            }),
            bench("lateral per block", || {
                observed(run(&rows, Policy::Lateral))
            }),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Independent => "independent predicates",
        Shape::Correlated => "correlated redundant rejects",
        Shape::AntiCorrelated => "anti-correlated rejects",
        Shape::Drift => "mid-query regime reversal",
        Shape::FrontLoadedBias => "misleading block prefixes",
    }
}

fn generate(shape: Shape) -> Vec<[bool; 4]> {
    let mut random = Lcg::new(0x1A7E_0043_u64);
    (0..ROWS)
        .map(|row| {
            let a = random.below(100);
            let b = random.below(100);
            let c = random.below(100);
            match shape {
                Shape::Independent => [a < 75, b < 45, c < 20, random.below(100) < 65],
                Shape::Correlated => {
                    let shared = a < 25;
                    [!shared, !shared && b < 92, !shared && c < 70, b < 70]
                }
                Shape::AntiCorrelated => [a >= 20, !(20..45).contains(&a), a < 70, b < 75],
                Shape::Drift => match (row / BLOCK) % 4 {
                    0 => [a < 5, b < 95, c < 95, random.below(100) < 95],
                    1 => [a < 95, b < 5, c < 95, random.below(100) < 95],
                    2 => [a < 95, b < 95, c < 5, random.below(100) < 95],
                    _ => [a < 95, b < 95, c < 95, random.below(100) < 5],
                },
                Shape::FrontLoadedBias => {
                    if row % BLOCK < SAMPLE {
                        [a < 95, b < 5, c < 95, random.below(100) < 95]
                    } else {
                        [a < 5, b < 95, c < 95, random.below(100) < 95]
                    }
                }
            }
        })
        .collect()
}

fn run(rows: &[[bool; 4]], policy: Policy) -> Outcome {
    let static_order = match policy {
        Policy::Sql => [3, 2, 1, 0],
        Policy::StaticLearned => choose_order(&rows[..BLOCK.min(rows.len())]),
        Policy::OracleStatic => best_permutation(rows),
        Policy::Lateral => [0, 1, 2, 3],
    };
    let mut matches = 0_u64;
    let mut matched_positions = Vec::new();
    let mut work = 0_u64;
    let mut sampled_rows = 0_u64;

    for (block_index, block) in rows.chunks(BLOCK).enumerate() {
        let (order, skip) = if matches!(policy, Policy::Lateral) {
            // A prefix sample can be adversarially unrepresentative (sorted or
            // clustered blocks are common in an analytical store). Sample at
            // equal strides and remember those rows so they are not evaluated
            // twice. All four predicate costs are charged for every sample.
            let sample_count = SAMPLE.min(block.len());
            let sample_offsets = (0..sample_count)
                .map(|sample| sample * block.len() / sample_count)
                .collect::<Vec<_>>();
            let sample = sample_offsets
                .iter()
                .map(|offset| block[*offset])
                .collect::<Vec<_>>();
            work += sample_count as u64 * COST.iter().sum::<u64>();
            sampled_rows += sample_count as u64;
            let order = choose_order(&sample);
            for (&offset, row) in sample_offsets.iter().zip(&sample) {
                if row.iter().all(|value| *value) {
                    matches += 1;
                    let position = (block_index * BLOCK + offset) as u64;
                    matched_positions.push(position);
                }
            }
            (order, Some(sample_offsets))
        } else {
            (static_order, None)
        };

        for (offset, row) in block.iter().enumerate() {
            if skip
                .as_ref()
                .is_some_and(|offsets| offsets.binary_search(&offset).is_ok())
            {
                continue;
            }
            let mut accepted = true;
            for predicate in order {
                work += COST[predicate];
                if !row[predicate] {
                    accepted = false;
                    break;
                }
            }
            if accepted {
                matches += 1;
                let position = (block_index * BLOCK + offset) as u64;
                matched_positions.push(position);
            }
        }
    }
    matched_positions.sort_unstable();
    let mut checksum = matched_positions
        .into_iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |checksum, position| {
            (checksum ^ position).wrapping_mul(0x100_0000_01b3)
        });
    checksum ^= matches.rotate_left(19);
    Outcome {
        checksum,
        matches,
        work,
        sampled_rows,
    }
}

fn observed(outcome: Outcome) -> u64 {
    let checksum = outcome.checksum;
    black_box(outcome);
    checksum
}

fn choose_order(sample: &[[bool; 4]]) -> [usize; 4] {
    let mut chosen = [usize::MAX; 4];
    let mut survivors = vec![true; sample.len()];
    for slot in 0..4 {
        let predicate = (0..4)
            .filter(|candidate| !chosen[..slot].contains(candidate))
            .max_by(|left, right| {
                marginal_score(*left, sample, &survivors)
                    .total_cmp(&marginal_score(*right, sample, &survivors))
            })
            .expect("one unselected predicate remains");
        chosen[slot] = predicate;
        for (alive, row) in survivors.iter_mut().zip(sample) {
            *alive &= row[predicate];
        }
    }
    chosen
}

fn marginal_score(predicate: usize, sample: &[[bool; 4]], survivors: &[bool]) -> f64 {
    let rejected = sample
        .iter()
        .zip(survivors)
        .filter(|(row, alive)| **alive && !row[predicate])
        .count();
    rejected as f64 / COST[predicate] as f64
}

fn best_permutation(rows: &[[bool; 4]]) -> [usize; 4] {
    let mut best = [0, 1, 2, 3];
    let mut best_work = u64::MAX;
    for a in 0..4 {
        for b in 0..4 {
            for c in 0..4 {
                for d in 0..4 {
                    let order = [a, b, c, d];
                    if !all_distinct(order) {
                        continue;
                    }
                    let work = modeled_work(rows, order);
                    if work < best_work {
                        best_work = work;
                        best = order;
                    }
                }
            }
        }
    }
    best
}

fn modeled_work(rows: &[[bool; 4]], order: [usize; 4]) -> u64 {
    rows.iter()
        .map(|row| {
            let mut work = 0;
            for predicate in order {
                work += COST[predicate];
                if !row[predicate] {
                    break;
                }
            }
            work
        })
        .sum()
}

fn all_distinct(order: [usize; 4]) -> bool {
    order[0] != order[1]
        && order[0] != order[2]
        && order[0] != order[3]
        && order[1] != order[2]
        && order[1] != order[3]
        && order[2] != order[3]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_policy_returns_the_same_exact_rows() {
        for shape in [
            Shape::Independent,
            Shape::Correlated,
            Shape::AntiCorrelated,
            Shape::Drift,
            Shape::FrontLoadedBias,
        ] {
            let rows = generate(shape);
            let expected = run(&rows, Policy::Sql);
            for policy in [Policy::StaticLearned, Policy::OracleStatic, Policy::Lateral] {
                let actual = run(&rows, policy);
                assert_eq!(actual.matches, expected.matches);
                assert_eq!(actual.checksum, expected.checksum);
            }
        }
    }

    #[test]
    fn stratified_sampling_resists_a_misleading_prefix() {
        let rows = generate(Shape::FrontLoadedBias);
        let static_best = run(&rows, Policy::OracleStatic);
        let lateral = run(&rows, Policy::Lateral);
        assert!(lateral.work <= static_best.work * 105 / 100);
    }

    #[test]
    fn block_adaptation_repays_sampling_under_drift() {
        let rows = generate(Shape::Drift);
        let static_best = run(&rows, Policy::OracleStatic);
        let lateral = run(&rows, Policy::Lateral);
        assert!(lateral.work * 100 <= static_best.work * 80);
    }
}
