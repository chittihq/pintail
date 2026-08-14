//! Prototype e56: adapt memory conductance from observed marginal spill savings.

use common::{Lcg, bench};

const OPS: usize = 4;
const CAP: u64 = 1_000;
const FLOOR: u64 = 100;
const EPOCHS: usize = 2_000;

#[derive(Clone, Copy)]
enum Shape {
    Stable,
    Reversal,
    Bursts,
}
#[derive(Clone, Copy)]
enum Policy {
    Equal,
    FirstCome,
    Static,
    Vascular,
}
#[derive(Clone, Copy)]
struct Epoch {
    demand: [u64; OPS],
    utility: [u64; OPS],
    token: u64,
}
struct Outcome {
    checksum: u64,
    spill: u64,
    p95: u64,
    peak: u64,
    min_grant: u64,
    recovery: Option<u64>,
}

fn main() {
    println!("e56 — vascular memory flow (executable prototype, audited)");
    for shape in [Shape::Stable, Shape::Reversal, Shape::Bursts] {
        let trace = fixture(shape);
        let policies = [
            ("equal share", Policy::Equal),
            ("first come", Policy::FirstCome),
            ("static weights", Policy::Static),
            ("vascular", Policy::Vascular),
        ];
        println!("\n=== {} ===", shape_name(shape));
        for (name, policy) in policies {
            let out = execute(&trace, policy, shape);
            println!(
                "{name:<16} spill {:>10} p95 {:>6} cap {}/{} floor {} recovery {:?} ck {:016x}",
                out.spill, out.p95, out.peak, CAP, out.min_grant, out.recovery, out.checksum
            );
        }
        for (name, policy) in policies {
            let _ = bench(name, || execute(&trace, policy, shape).checksum);
        }
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Stable => "stable curves",
        Shape::Reversal => "utility reversal",
        Shape::Bursts => "bursts",
    }
}

fn fixture(shape: Shape) -> Vec<Epoch> {
    let mut random = Lcg::new(0x5600_0056 ^ shape as u64);
    (0..EPOCHS)
        .map(|tick| {
            let mut utility = [30, 80, 150, 250];
            if matches!(shape, Shape::Reversal) && tick >= EPOCHS / 2 {
                utility.reverse();
            }
            if matches!(shape, Shape::Bursts) && (tick / 100).is_multiple_of(2) {
                utility.rotate_left(1);
            }
            for value in &mut utility {
                *value += random.below(20);
            }
            let demand = std::array::from_fn(|op| 180 + op as u64 * 40 + random.below(180));
            Epoch {
                demand,
                utility,
                token: random.next_u64(),
            }
        })
        .collect()
}

fn allocate(epoch: &Epoch, policy: Policy, conductance: &[u64; OPS]) -> [u64; OPS] {
    let mut grant = [FLOOR; OPS];
    let mut free = CAP - FLOOR * OPS as u64;
    while free > 0 {
        let eligible = (0..OPS)
            .filter(|op| grant[*op] < epoch.demand[*op])
            .collect::<Vec<_>>();
        if eligible.is_empty() {
            break;
        }
        let op = match policy {
            Policy::Equal => *eligible
                .iter()
                .min_by_key(|op| (grant[**op], **op))
                .unwrap(),
            Policy::FirstCome => eligible[0],
            Policy::Static => *eligible
                .iter()
                .max_by_key(|op| ([1, 2, 3, 4][**op], std::cmp::Reverse(**op)))
                .unwrap(),
            Policy::Vascular => *eligible
                .iter()
                .max_by_key(|op| (conductance[**op], std::cmp::Reverse(**op)))
                .unwrap(),
        };
        let add = 10.min(free).min(epoch.demand[op] - grant[op]);
        grant[op] += add;
        free -= add;
    }
    grant
}

fn execute(trace: &[Epoch], policy: Policy, shape: Shape) -> Outcome {
    let mut conductance = [100_u64; OPS];
    let mut spill = 0;
    let mut latencies = Vec::new();
    let mut checksum = 0_u64;
    let mut peak = 0;
    let mut min_grant = u64::MAX;
    let mut recovery = None;
    for (tick, epoch) in trace.iter().enumerate() {
        let grant = allocate(epoch, policy, &conductance);
        let used = grant.iter().sum::<u64>();
        assert!(used <= CAP);
        peak = peak.max(used);
        min_grant = min_grant.min(*grant.iter().min().unwrap());
        let mut latency = 0;
        for op in 0..OPS {
            let missing = epoch.demand[op] - grant[op];
            spill += missing * epoch.utility[op];
            latency = latency.max(100 + missing * epoch.utility[op] / 10);
            checksum = checksum.rotate_left(5) ^ grant[op] ^ epoch.token.rotate_left(op as u32);
            if matches!(policy, Policy::Vascular) {
                conductance[op] = (conductance[op] * 7 + epoch.utility[op]) / 8;
            }
        }
        latencies.push(latency);
        if matches!(shape, Shape::Reversal) && tick >= EPOCHS / 2 && recovery.is_none() {
            let best = epoch
                .utility
                .iter()
                .enumerate()
                .max_by_key(|(_, utility)| *utility)
                .unwrap()
                .0;
            if conductance[best] == *conductance.iter().max().unwrap() {
                recovery = Some((tick - EPOCHS / 2) as u64);
            }
        }
    }
    latencies.sort_unstable();
    Outcome {
        checksum,
        spill,
        p95: latencies[latencies.len() * 95 / 100],
        peak,
        min_grant,
        recovery,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_allocator_obeys_cap_floor_and_demand() {
        let trace = fixture(Shape::Bursts);
        for epoch in trace.iter().take(100) {
            for policy in [
                Policy::Equal,
                Policy::FirstCome,
                Policy::Static,
                Policy::Vascular,
            ] {
                let grant = allocate(epoch, policy, &[100; OPS]);
                assert!(grant.iter().sum::<u64>() <= CAP);
                for (granted, demand) in grant.into_iter().zip(epoch.demand) {
                    assert!(granted >= FLOOR && granted <= demand);
                }
            }
        }
    }
    #[test]
    fn vascular_state_uses_observed_utility_and_recovers() {
        let out = execute(&fixture(Shape::Reversal), Policy::Vascular, Shape::Reversal);
        assert!(out.recovery.is_some());
    }
}
