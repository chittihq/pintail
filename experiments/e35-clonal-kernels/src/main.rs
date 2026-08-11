//! e35: retain and clone safe kernels by cheap block context instead of one winner.
//!
//! Evidence tier: contextual-kernel policy simulation with charged exploration.

use common::{Lcg, bench, check_consistency};

const CALLS: usize = 60_000;
const CONTEXTS: usize = 12;
const KERNELS: usize = 4;

#[derive(Clone, Copy)]
enum Shape {
    Stable,
    Drift,
    RareContexts,
}

#[derive(Clone, Copy)]
struct Call {
    context: usize,
    cost: [u64; KERNELS],
    token: u64,
}

#[derive(Clone, Copy)]
enum Policy {
    Global,
    Manual,
    Epsilon,
    Clonal,
}

struct Outcome {
    checksum: u64,
    work: u64,
    oracle: u64,
    exploration: u64,
    min_validations: u64,
}

fn main() {
    println!("e35 — clonal kernel repertoire (simulation tier)");
    println!("{CALLS} calls, {CONTEXTS} contexts, {KERNELS} safe kernels\n");
    for shape in [Shape::Stable, Shape::Drift, Shape::RareContexts] {
        let trace = trace(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("one global winner", Policy::Global),
            ("manual thresholds", Policy::Manual),
            ("epsilon per-context", Policy::Epsilon),
            ("clonal aging repertoire", Policy::Clonal),
        ];
        let mut expected = None;
        for (name, policy) in policies {
            let outcome = run(&trace, policy);
            if let Some(checksum) = expected {
                assert_eq!(checksum, outcome.checksum);
            } else {
                expected = Some(outcome.checksum);
            }
            println!(
                "{name:<25} work {:>10}  vs oracle {:>6.2}%  explore {:>5}  min validate {:>3}",
                outcome.work,
                outcome.work as f64 * 100.0 / outcome.oracle as f64,
                outcome.exploration,
                outcome.min_validations
            );
        }
        let results = [
            bench("global", || run(&trace, Policy::Global).checksum),
            bench("manual", || run(&trace, Policy::Manual).checksum),
            bench("epsilon", || run(&trace, Policy::Epsilon).checksum),
            bench("clonal", || run(&trace, Policy::Clonal).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Stable => "stable diverse blocks",
        Shape::Drift => "mid-run architecture/data reversal",
        Shape::RareContexts => "90/10 common and rare contexts",
    }
}

fn trace(shape: Shape) -> Vec<Call> {
    let mut random = Lcg::new(0xC10A_0035_u64);
    (0..CALLS)
        .map(|index| {
            let context = if matches!(shape, Shape::RareContexts) && random.below(100) < 90 {
                random.below(3) as usize
            } else {
                random.below(CONTEXTS as u64) as usize
            };
            let rotated = matches!(shape, Shape::Drift) && index >= CALLS / 2;
            let winner = (context + usize::from(rotated)) % KERNELS;
            let mut cost = [0_u64; KERNELS];
            for (kernel, value) in cost.iter_mut().enumerate() {
                *value = if kernel == winner {
                    100
                } else {
                    125 + ((kernel + CONTEXTS - winner) % KERNELS) as u64 * 25
                };
            }
            Call {
                context,
                cost,
                token: random.next_u64(),
            }
        })
        .collect()
}

fn run(trace: &[Call], policy: Policy) -> Outcome {
    let oracle = trace
        .iter()
        .map(|call| *call.cost.iter().min().expect("kernels"))
        .sum::<u64>();
    let global = (0..KERNELS)
        .min_by_key(|kernel| trace.iter().map(|call| call.cost[*kernel]).sum::<u64>())
        .expect("kernels");
    let mut means = [[[160.0_f64; KERNELS]; CONTEXTS]; 2];
    let mut counts = [[[0_u64; KERNELS]; CONTEXTS]; 2];
    let mut random = Lcg::new(0x5C07_0035_u64);
    let mut work = 0_u64;
    let mut exploration = 0_u64;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;

    for (index, call) in trace.iter().enumerate() {
        let bank = usize::from(index >= CALLS / 2);
        let explore = match policy {
            Policy::Epsilon => random.below(100) < 5,
            Policy::Clonal => random.below(100) < 3,
            _ => false,
        };
        let kernel = match policy {
            Policy::Global => global,
            Policy::Manual => call.context % KERNELS,
            Policy::Epsilon | Policy::Clonal => {
                if let Some(unseen) = counts[bank][call.context]
                    .iter()
                    .position(|count| *count == 0)
                {
                    unseen
                } else if explore {
                    random.below(KERNELS as u64) as usize
                } else {
                    means[bank][call.context]
                        .iter()
                        .enumerate()
                        .min_by(|(_, left), (_, right)| left.total_cmp(right))
                        .map(|(kernel, _)| kernel)
                        .expect("kernels")
                }
            }
        };
        exploration += u64::from(explore);
        work += call.cost[kernel];
        checksum = (checksum ^ call.token).wrapping_mul(0x100_0000_01b3);
        counts[bank][call.context][kernel] += 1;
        let alpha = if matches!(policy, Policy::Clonal) {
            0.12
        } else {
            1.0 / counts[bank][call.context][kernel].min(200) as f64
        };
        means[bank][call.context][kernel] =
            means[bank][call.context][kernel] * (1.0 - alpha) + call.cost[kernel] as f64 * alpha;
    }
    let min_validations = if matches!(policy, Policy::Epsilon | Policy::Clonal) {
        counts
            .iter()
            .flat_map(|bank| bank.iter())
            .flat_map(|context| context.iter())
            .copied()
            .filter(|count| *count > 0)
            .min()
            .unwrap_or(0)
    } else {
        0
    };
    Outcome {
        checksum,
        work,
        oracle,
        exploration,
        min_validations,
    }
}
