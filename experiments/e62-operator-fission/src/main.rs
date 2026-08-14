//! Prototype e62: split and merge exact aggregate state from sampled skew.

use common::{Lcg, bench};

const GROUPS: usize = 1_024;
const SHARDS: usize = 8;

#[derive(Clone, Copy)]
enum Shape {
    Uniform,
    Skew,
    Shifting,
    Tiny,
}
#[derive(Clone, Copy)]
enum Policy {
    Global,
    Sharded,
    SplitOnly,
    Reversible,
}
struct Outcome {
    checksum: u64,
    cost: u64,
    transitions: u64,
    transition_work: u64,
    phase_costs: Vec<u64>,
}

fn main() {
    println!("e62 — reversible operator fission/fusion (executable prototype, audited)");
    for shape in [Shape::Uniform, Shape::Skew, Shape::Shifting, Shape::Tiny] {
        let phases = fixture(shape);
        let policies = [
            ("always global", Policy::Global),
            ("always sharded", Policy::Sharded),
            ("one-way split", Policy::SplitOnly),
            ("reversible", Policy::Reversible),
        ];
        let outcomes = policies.map(|(_, policy)| execute(&phases, policy));
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.checksum == outcomes[0].checksum)
        );
        println!("\n=== {} ===", shape_name(shape));
        for ((name, _), out) in policies.iter().zip(outcomes.iter()) {
            println!(
                "{name:<18} cost {:>8} phases {:?} transitions {} work {} ck {:016x}",
                out.cost, out.phase_costs, out.transitions, out.transition_work, out.checksum
            );
        }
        for (name, policy) in policies {
            let _ = bench(name, || execute(&phases, policy).checksum);
        }
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Uniform => "uniform",
        Shape::Skew => "skewed",
        Shape::Shifting => "shifting skew",
        Shape::Tiny => "tiny",
    }
}
fn phase(random: &mut Lcg, rows: usize, skewed: bool) -> Vec<u16> {
    (0..rows)
        .map(|_| {
            if skewed && random.below(100) < 80 {
                7
            } else {
                random.below(GROUPS as u64) as u16
            }
        })
        .collect()
}
fn fixture(shape: Shape) -> Vec<Vec<u16>> {
    let mut random = Lcg::new(0x6200_0062 ^ shape as u64);
    match shape {
        Shape::Uniform => vec![phase(&mut random, 20_000, false); 4],
        Shape::Skew => vec![phase(&mut random, 20_000, true); 4],
        Shape::Shifting => vec![
            phase(&mut random, 20_000, false),
            phase(&mut random, 20_000, true),
            phase(&mut random, 20_000, false),
            phase(&mut random, 20_000, true),
        ],
        Shape::Tiny => vec![phase(&mut random, 1_000, false); 4],
    }
}

fn sample_wants_shards(rows: &[u16]) -> bool {
    let mut counts = [0_u16; 64];
    for key in rows.iter().take(256) {
        counts[*key as usize % 64] += 1;
    }
    counts.into_iter().max().unwrap() > 80
}
fn aggregate(rows: &[u16], sharded: bool) -> ([u64; GROUPS], u64) {
    let mut result = [0_u64; GROUPS];
    if sharded {
        let mut shards = vec![[0_u64; GROUPS]; SHARDS];
        for (row, key) in rows.iter().enumerate() {
            shards[row % SHARDS][*key as usize] += 1;
        }
        for shard in shards {
            for (group, count) in shard.into_iter().enumerate() {
                result[group] += count;
            }
        }
    } else {
        for key in rows {
            result[*key as usize] += 1;
        }
    }
    let hottest = *result.iter().max().unwrap();
    let cost = if sharded {
        rows.len() as u64 + 4_000
    } else {
        rows.len() as u64 + hottest * 4
    };
    (result, cost)
}

fn execute(phases: &[Vec<u16>], policy: Policy) -> Outcome {
    let mut sharded = matches!(policy, Policy::Sharded);
    let mut transitions = 0;
    let mut transition_work = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut phase_costs = Vec::new();
    for (phase, rows) in phases.iter().enumerate() {
        let want = sample_wants_shards(rows);
        let next = match policy {
            Policy::Global => false,
            Policy::Sharded => true,
            Policy::SplitOnly => sharded || want,
            Policy::Reversible => want,
        };
        if next != sharded {
            transitions += 1;
            transition_work += GROUPS as u64;
            sharded = next;
        }
        let (groups, mut cost) = aggregate(rows, sharded);
        cost += u64::from(matches!(policy, Policy::SplitOnly | Policy::Reversible)) * 256;
        phase_costs.push(cost);
        for (group, count) in groups.into_iter().enumerate() {
            checksum = (checksum ^ count ^ (group as u64 + 1) ^ (phase as u64).rotate_left(17))
                .wrapping_mul(0x100_0000_01b3);
        }
    }
    Outcome {
        checksum,
        cost: phase_costs.iter().sum::<u64>() + transition_work,
        transitions,
        transition_work,
        phase_costs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_states_merge_to_the_exact_same_aggregate() {
        for shape in [Shape::Uniform, Shape::Skew, Shape::Shifting, Shape::Tiny] {
            let phases = fixture(shape);
            let expected = execute(&phases, Policy::Global).checksum;
            for policy in [Policy::Sharded, Policy::SplitOnly, Policy::Reversible] {
                assert_eq!(execute(&phases, policy).checksum, expected);
            }
        }
    }
    #[test]
    fn transition_work_is_below_five_percent() {
        let phases = fixture(Shape::Shifting);
        let out = execute(&phases, Policy::Reversible);
        assert!(out.transition_work * 100 < phases.iter().map(Vec::len).sum::<usize>() as u64 * 5);
    }
}
