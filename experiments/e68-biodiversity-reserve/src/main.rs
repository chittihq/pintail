//! e68: reserve tiny validation traffic for a runner-up kernel under drift.
//!
//! Evidence tier: deterministic reversal simulation with charged probes.

use common::{Lcg, bench, check_consistency};

const CALLS: usize = 80_000;

#[derive(Clone, Copy)]
enum Shape {
    Stable,
    OneReversal,
    Volatile,
}

#[derive(Clone, Copy)]
enum Policy {
    Permanent,
    Periodic,
    Epsilon,
    Diversity,
}

struct Outcome {
    checksum: u64,
    work: u64,
    oracle: u64,
    detection: usize,
    switches: u64,
    exploration: u64,
}

fn main() {
    println!("e68 — algorithmic biodiversity reserve (simulation tier)");
    println!("{CALLS} kernel calls; 2% diversity traffic\n");
    for shape in [Shape::Stable, Shape::OneReversal, Shape::Volatile] {
        let costs = costs(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("permanent winner", Policy::Permanent),
            ("periodic benchmark", Policy::Periodic),
            ("epsilon 5%", Policy::Epsilon),
            ("diversity reserve 2%", Policy::Diversity),
        ];
        let mut expected = None;
        for (name, policy) in policies {
            let outcome = run(&costs, policy);
            if let Some(checksum) = expected {
                assert_eq!(checksum, outcome.checksum);
            } else {
                expected = Some(outcome.checksum);
            }
            println!(
                "{name:<25} work {:>10}  regret {:>8}  detect {:>4}  switches {:>3}  probes {:>5}",
                outcome.work,
                outcome.work - outcome.oracle,
                outcome.detection,
                outcome.switches,
                outcome.exploration
            );
        }
        let results = [
            bench("permanent", || run(&costs, Policy::Permanent).checksum),
            bench("periodic", || run(&costs, Policy::Periodic).checksum),
            bench("epsilon", || run(&costs, Policy::Epsilon).checksum),
            bench("diversity", || run(&costs, Policy::Diversity).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Stable => "stable winner",
        Shape::OneReversal => "one persistent reversal",
        Shape::Volatile => "four genuine reversals",
    }
}

fn costs(shape: Shape) -> Vec<[u64; 2]> {
    (0..CALLS)
        .map(|index| {
            let phase = match shape {
                Shape::Stable => 0,
                Shape::OneReversal => usize::from(index >= CALLS / 2),
                Shape::Volatile => index / (CALLS / 5),
            };
            if phase.is_multiple_of(2) {
                [100, 135]
            } else {
                [145, 95]
            }
        })
        .collect()
}

fn run(costs: &[[u64; 2]], policy: Policy) -> Outcome {
    let oracle = costs.iter().map(|cost| cost[0].min(cost[1])).sum::<u64>();
    let mut active = 0_usize;
    let mut estimates = [100.0_f64, 135.0];
    let mut observations = [1_u64, 1];
    let mut random = Lcg::new(0xD1A0_0068_u64);
    let mut work = 0_u64;
    let mut switches = 0_u64;
    let mut exploration = 0_u64;
    let mut detection = 0_usize;
    let mut pending_reversal = None;
    let mut last_oracle = 0_usize;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;

    for (index, cost) in costs.iter().enumerate() {
        let oracle_arm = usize::from(cost[1] < cost[0]);
        if oracle_arm != last_oracle {
            pending_reversal = Some(index);
            last_oracle = oracle_arm;
        }
        let probe = match policy {
            Policy::Permanent => false,
            Policy::Periodic => index % 100 == 0,
            Policy::Epsilon => random.below(100) < 5,
            Policy::Diversity => random.below(100) < 2,
        };
        let arm = if probe { 1 - active } else { active };
        exploration += u64::from(probe);
        work += cost[arm];
        checksum = (checksum ^ index as u64).wrapping_mul(0x100_0000_01b3);
        observations[arm] += 1;
        let alpha = if matches!(policy, Policy::Diversity) {
            0.08
        } else {
            1.0 / observations[arm].min(100) as f64
        };
        estimates[arm] = estimates[arm] * (1.0 - alpha) + cost[arm] as f64 * alpha;

        if !matches!(policy, Policy::Permanent) && estimates[1 - active] * 1.02 < estimates[active]
        {
            active = 1 - active;
            switches += 1;
            if let Some(start) = pending_reversal.take() {
                detection = detection.max(index - start);
            }
        }
    }
    Outcome {
        checksum,
        work,
        oracle,
        detection,
        switches,
        exploration,
    }
}
