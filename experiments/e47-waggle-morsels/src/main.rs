//! Prototype e47: schedule morsels from observable yield advertisements plus scouts.

use common::{Lcg, bench};

const MORSELS: usize = 512;
const WORKERS: usize = 8;

#[derive(Clone, Copy)]
enum Shape {
    Clustered,
    Moving,
    Costly,
    Uniform,
}

#[derive(Clone, Copy)]
enum Policy {
    Fifo,
    Random,
    Density,
    Waggle,
}

#[derive(Clone, Copy)]
struct Morsel {
    matches: u32,
    cost: u64,
    hint: u32,
    token: u64,
}

struct Outcome {
    checksum: u64,
    first: u64,
    completion: u64,
}

fn main() {
    println!("e47 — waggle morsel discovery (executable prototype, audited)");
    for shape in [
        Shape::Clustered,
        Shape::Moving,
        Shape::Costly,
        Shape::Uniform,
    ] {
        let morsels = fixture(shape);
        let policies = [
            ("FIFO", Policy::Fifo),
            ("random steal", Policy::Random),
            ("density hint", Policy::Density),
            ("waggle + scouts", Policy::Waggle),
        ];
        let outcomes = policies.map(|(_, policy)| execute(&morsels, policy));
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.checksum == outcomes[0].checksum)
        );
        println!("\n=== {} ===", shape_name(shape));
        for ((name, _), outcome) in policies.iter().zip(outcomes.iter()) {
            println!(
                "{name:<18} first {:>6} completion {:>6} checksum {:016x}",
                outcome.first, outcome.completion, outcome.checksum
            );
        }
        for (name, policy) in policies {
            let _ = bench(name, || execute(&morsels, policy).checksum);
        }
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Clustered => "clustered sparse",
        Shape::Moving => "two moving clusters",
        Shape::Costly => "costly UDF",
        Shape::Uniform => "uniform control",
    }
}

fn fixture(shape: Shape) -> Vec<Morsel> {
    let mut random = Lcg::new(0x4700_0047 ^ shape as u64);
    (0..MORSELS)
        .map(|index| {
            let in_cluster = match shape {
                Shape::Clustered => (340..380).contains(&index),
                Shape::Moving => (120..145).contains(&index) || (410..435).contains(&index),
                Shape::Costly => (300..350).contains(&index),
                Shape::Uniform => true,
            };
            let matches = if in_cluster {
                5 + random.below(20) as u32
            } else {
                0
            };
            let cost = if matches!(shape, Shape::Costly) {
                20 + random.below(100)
            } else {
                20 + random.below(10)
            };
            let noisy_signal = if in_cluster { 30 } else { 0 } + random.below(20) as u32;
            Morsel {
                matches,
                cost,
                hint: noisy_signal,
                token: random.next_u64(),
            }
        })
        .collect()
}

fn choose(
    policy: Policy,
    processed: &[bool],
    hint: &[u32],
    random_rank: &[u64],
    pick: usize,
) -> usize {
    let remaining = (0..MORSELS).filter(|index| !processed[*index]);
    match policy {
        Policy::Fifo => remaining.min().expect("remaining morsel"),
        Policy::Random => remaining
            .min_by_key(|index| random_rank[*index])
            .expect("remaining morsel"),
        Policy::Density => remaining
            .max_by_key(|index| (hint[*index], std::cmp::Reverse(*index)))
            .expect("remaining morsel"),
        Policy::Waggle if pick.is_multiple_of(4) => remaining
            .min_by_key(|index| random_rank[*index])
            .expect("scout morsel"),
        Policy::Waggle => remaining
            .max_by_key(|index| (hint[*index], std::cmp::Reverse(*index)))
            .expect("advertised morsel"),
    }
}

fn execute(morsels: &[Morsel], policy: Policy) -> Outcome {
    let mut processed = vec![false; MORSELS];
    let mut scores = morsels.iter().map(|morsel| morsel.hint).collect::<Vec<_>>();
    let mut random = Lcg::new(0x4711);
    let random_rank = (0..MORSELS).map(|_| random.next_u64()).collect::<Vec<_>>();
    let mut checksum = 0_u64;
    let mut first = None;
    let mut completion = 0_u64;
    let mut done = 0;
    while done < MORSELS {
        let mut batch = Vec::new();
        for pick in 0..WORKERS.min(MORSELS - done) {
            let index = choose(policy, &processed, &scores, &random_rank, pick);
            processed[index] = true;
            batch.push(index);
        }
        if first.is_none() {
            first = batch
                .iter()
                .filter(|index| morsels[**index].matches > 0)
                .map(|index| completion + morsels[*index].cost)
                .min();
        }
        completion += batch
            .iter()
            .map(|index| morsels[*index].cost)
            .max()
            .unwrap_or(0);
        for index in batch {
            let morsel = morsels[index];
            checksum ^= morsel
                .token
                .wrapping_mul(u64::from(morsel.matches))
                .rotate_left((index % 63) as u32);
            if matches!(policy, Policy::Waggle) && morsel.matches > 0 {
                let quality = morsel.matches.saturating_mul(100) / morsel.cost as u32;
                for neighbor in index.saturating_sub(8)..(index + 9).min(MORSELS) {
                    if !processed[neighbor] {
                        scores[neighbor] = scores[neighbor].saturating_add(quality);
                    }
                }
            }
            done += 1;
        }
    }
    Outcome {
        checksum,
        first: first.unwrap_or(completion),
        completion,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_schedule_computes_the_same_result() {
        for shape in [
            Shape::Clustered,
            Shape::Moving,
            Shape::Costly,
            Shape::Uniform,
        ] {
            let morsels = fixture(shape);
            let expected = execute(&morsels, Policy::Fifo).checksum;
            for policy in [Policy::Random, Policy::Density, Policy::Waggle] {
                assert_eq!(execute(&morsels, policy).checksum, expected);
            }
        }
    }
    #[test]
    fn uniform_control_stays_within_three_percent_of_fifo() {
        let morsels = fixture(Shape::Uniform);
        let fifo = execute(&morsels, Policy::Fifo);
        let waggle = execute(&morsels, Policy::Waggle);
        assert!(waggle.completion * 100 <= fifo.completion * 103);
    }
}
