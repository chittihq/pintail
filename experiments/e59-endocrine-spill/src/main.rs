//! e59: coordinate operator memory from a global pressure signal plus fast reflex.
//!
//! Evidence tier: deterministic allocation simulation. Missing ideal memory maps
//! to spill bytes via a fixed operator-specific marginal-utility curve.

use common::{Lcg, bench, check_consistency};

const EPOCHS: usize = 20_000;
const OPERATORS: usize = 4;
const CAP: u32 = 1_000;

#[derive(Clone, Copy)]
enum Shape {
    Balanced,
    Heterogeneous,
    Burst,
    Drift,
}

#[derive(Clone, Copy)]
struct Epoch {
    demand: [u32; OPERATORS],
    utility: [u32; OPERATORS],
}

#[derive(Clone, Copy)]
enum Policy {
    Independent,
    EqualHard,
    LaggedPid,
    HormoneReflex,
}

struct Outcome {
    checksum: u64,
    spill: u64,
    p99_latency: u64,
    storm_epochs: u64,
    unused: u64,
}

fn main() {
    println!("e59 — endocrine spill coordination (simulation tier)");
    println!("{EPOCHS} epochs, {OPERATORS} concurrent operators, memory cap {CAP}\n");
    for shape in [
        Shape::Balanced,
        Shape::Heterogeneous,
        Shape::Burst,
        Shape::Drift,
    ] {
        let trace = trace(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("independent thresholds", Policy::Independent),
            ("global equal hard cap", Policy::EqualHard),
            ("lagged pressure PID", Policy::LaggedPid),
            ("hormone + fast reflex", Policy::HormoneReflex),
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
                "{name:<25} spill {:>13}  p99 {:>7}  storms {:>6}  unused {:>11}",
                outcome.spill, outcome.p99_latency, outcome.storm_epochs, outcome.unused
            );
        }
        let results = [
            bench("independent", || run(&trace, Policy::Independent).checksum),
            bench("equal", || run(&trace, Policy::EqualHard).checksum),
            bench("PID", || run(&trace, Policy::LaggedPid).checksum),
            bench("hormone", || run(&trace, Policy::HormoneReflex).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Balanced => "balanced low pressure",
        Shape::Heterogeneous => "different marginal utilities",
        Shape::Burst => "synchronized memory bursts",
        Shape::Drift => "utility reversal",
    }
}

fn trace(shape: Shape) -> Vec<Epoch> {
    let mut random = Lcg::new(0xE0D0_0059_u64);
    (0..EPOCHS)
        .map(|index| {
            let phase = index / (EPOCHS / 4);
            match shape {
                Shape::Balanced => Epoch {
                    demand: [180, 190, 175, 185],
                    utility: [3, 3, 3, 3],
                },
                Shape::Heterogeneous => Epoch {
                    demand: [420, 380, 260, 190],
                    utility: [9, 6, 2, 1],
                },
                Shape::Burst => {
                    let high = index % 400 < 120;
                    Epoch {
                        demand: if high {
                            [480, 420, 390, 350]
                        } else {
                            [160, 150, 170, 155]
                        },
                        utility: [8, 5, 3, 2],
                    }
                }
                Shape::Drift => {
                    let mut utility = [2, 2, 2, 2];
                    utility[phase] = 10;
                    Epoch {
                        demand: [300 + random.below(81) as u32, 300, 300, 300],
                        utility,
                    }
                }
            }
        })
        .collect()
}

fn run(trace: &[Epoch], policy: Policy) -> Outcome {
    let mut previous_demand = [250_u32; OPERATORS];
    let mut latencies = Vec::with_capacity(trace.len());
    let mut spill = 0_u64;
    let mut storm_epochs = 0_u64;
    let mut unused = 0_u64;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;

    for (index, epoch) in trace.iter().enumerate() {
        let (allocation, storm) = allocate(*epoch, previous_demand, policy);
        let used = allocation.iter().sum::<u32>();
        assert!(used <= CAP, "fast reflex must enforce the cap");
        unused += u64::from(CAP - used);
        let mut epoch_spill = 0_u64;
        let mut latency = 100_u64;
        for (operator, allocated) in allocation.iter().enumerate() {
            let missing = epoch.demand[operator].saturating_sub(*allocated);
            let weighted = u64::from(missing) * u64::from(epoch.utility[operator]);
            epoch_spill += weighted;
            latency = latency.max(100 + weighted / 4);
        }
        if storm > 0 {
            epoch_spill += u64::from(storm) * OPERATORS as u64;
            latency += u64::from(storm);
            storm_epochs += 1;
        }
        spill += epoch_spill;
        latencies.push(latency);
        checksum =
            (checksum ^ index as u64 ^ epoch.demand.iter().map(|v| u64::from(*v)).sum::<u64>())
                .wrapping_mul(0x100_0000_01b3);
        previous_demand = epoch.demand;
    }
    latencies.sort_unstable();
    Outcome {
        checksum,
        spill,
        p99_latency: latencies[latencies.len() * 99 / 100],
        storm_epochs,
        unused,
    }
}

fn allocate(epoch: Epoch, previous: [u32; OPERATORS], policy: Policy) -> ([u32; OPERATORS], u32) {
    match policy {
        Policy::Independent => {
            let desired = epoch.demand.map(|value| value.min(320));
            let total = desired.iter().sum::<u32>();
            if total <= CAP {
                (desired, 0)
            } else {
                (scale_to_cap(desired), total - CAP)
            }
        }
        Policy::EqualHard => (
            epoch.demand.map(|value| value.min(CAP / OPERATORS as u32)),
            0,
        ),
        Policy::LaggedPid => {
            let total = previous.iter().sum::<u32>().max(1);
            let mut allocation = [0_u32; OPERATORS];
            for index in 0..OPERATORS {
                allocation[index] = epoch.demand[index].min(CAP * previous[index] / total);
            }
            (allocation, 0)
        }
        Policy::HormoneReflex => {
            let mut allocation = epoch.demand.map(|value| value.min(64));
            let mut left = CAP - allocation.iter().sum::<u32>();
            let mut order = [0, 1, 2, 3];
            order.sort_unstable_by_key(|index| std::cmp::Reverse(epoch.utility[*index]));
            for operator in order {
                let grant = left.min(epoch.demand[operator] - allocation[operator]);
                allocation[operator] += grant;
                left -= grant;
            }
            (allocation, 0)
        }
    }
}

fn scale_to_cap(values: [u32; OPERATORS]) -> [u32; OPERATORS] {
    let total = values.iter().sum::<u32>();
    let mut scaled = values.map(|value| value * CAP / total);
    let mut left = CAP - scaled.iter().sum::<u32>();
    for value in &mut scaled {
        if left == 0 {
            break;
        }
        *value += 1;
        left -= 1;
    }
    scaled
}
