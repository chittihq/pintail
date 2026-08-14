//! e70: auction released memory by measured marginal spill reduction.

use common::{Lcg, bench};

const OPERATORS: usize = 6;
const BUDGET: u64 = 1_000;
const BASE_GRANT: u64 = 50;
const QUANTUM: u64 = 25;

#[derive(Clone, Copy)]
enum Shape {
    Staggered,
    Bursty,
    Skewed,
}

#[derive(Clone, Copy)]
enum Policy {
    Fifo,
    Equal,
    Priority,
    Auction,
}

#[derive(Clone, Copy)]
struct Epoch {
    demand: [u64; OPERATORS],
    benefit: [u64; OPERATORS],
    token: u64,
}

struct Allocation {
    grant: [u64; OPERATORS],
    unspent: u64,
    bids: u64,
}

struct Outcome {
    checksum: u64,
    makespan: u64,
    spill: u64,
    peak: u64,
    max_wait: u64,
    bids: u64,
    unspent: u64,
}

fn main() {
    println!("e70 — forest-gap memory auctions (simulation tier, audited)");
    for shape in [Shape::Staggered, Shape::Bursty, Shape::Skewed] {
        let trace = trace(shape);
        println!("\n=== {} ===", shape_name(shape));
        let policies = [
            ("FIFO handoff", Policy::Fifo),
            ("equal redistribution", Policy::Equal),
            ("static priority", Policy::Priority),
            ("gap auction", Policy::Auction),
        ];
        for (name, policy) in policies {
            let outcome = run(&trace, policy);
            assert_eq!(outcome.unspent, 0, "{name} abandoned memory budget");
            println!(
                "{name:<21} makespan {:>9} spill {:>10} peak {:>4}/{BUDGET} wait {:>3} bids {:>6} unspent {}",
                outcome.makespan,
                outcome.spill,
                outcome.peak,
                outcome.max_wait,
                outcome.bids,
                outcome.unspent,
            );
        }
        for (name, policy) in policies {
            let _ = bench(name, || run(&trace, policy).checksum);
        }
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Staggered => "staggered operators",
        Shape::Bursty => "synchronized releases",
        Shape::Skewed => "non-linear benefits",
    }
}

fn trace(shape: Shape) -> Vec<Epoch> {
    let mut random = Lcg::new(0x7000_0070);
    (0..12_000)
        .map(|tick| {
            // The minimum aggregate demand is 1,080, so every policy can and must
            // consume the same 1,000-unit budget.
            let demand = std::array::from_fn(|operator| {
                180 + random.below(260)
                    + u64::from(matches!(shape, Shape::Bursty) && tick % 300 < 80) * 100
                    + u64::from(
                        matches!(shape, Shape::Staggered) && (tick + operator * 50) % 300 < 80,
                    ) * 100
            });
            let benefit = std::array::from_fn(|operator| {
                20 + random.below(180)
                    + u64::from(matches!(shape, Shape::Skewed) && operator == tick % OPERATORS)
                        * 400
            });
            Epoch {
                demand,
                benefit,
                token: random.next_u64(),
            }
        })
        .collect()
}

fn spill_cost(demand: u64, grant: u64, benefit: u64) -> u64 {
    let missing = demand.saturating_sub(grant);
    benefit * missing * missing / demand
}

fn marginal_saved(epoch: &Epoch, operator: usize, current: u64, add: u64) -> u64 {
    spill_cost(epoch.demand[operator], current, epoch.benefit[operator])
        - spill_cost(
            epoch.demand[operator],
            current + add,
            epoch.benefit[operator],
        )
}

fn allocate(epoch: &Epoch, policy: Policy, wait: &[u64; OPERATORS]) -> Allocation {
    let mut grant = [BASE_GRANT; OPERATORS];
    let mut free = BUDGET - BASE_GRANT * OPERATORS as u64;
    let mut bids = 0;

    while free > 0 {
        let eligible: Vec<_> = (0..OPERATORS)
            .filter(|operator| grant[*operator] < epoch.demand[*operator])
            .collect();
        if eligible.is_empty() {
            break;
        }

        let operator = match policy {
            Policy::Fifo => *eligible
                .iter()
                .max_by_key(|operator| (wait[**operator], std::cmp::Reverse(**operator)))
                .expect("eligible operator"),
            Policy::Equal => *eligible
                .iter()
                .min_by_key(|operator| (grant[**operator], **operator))
                .expect("eligible operator"),
            Policy::Priority => eligible[0],
            Policy::Auction => {
                bids += eligible.len() as u64;
                *eligible
                    .iter()
                    .max_by_key(|operator| {
                        let add = QUANTUM
                            .min(free)
                            .min(epoch.demand[**operator] - grant[**operator]);
                        (
                            marginal_saved(epoch, **operator, grant[**operator], add),
                            std::cmp::Reverse(**operator),
                        )
                    })
                    .expect("eligible operator")
            }
        };
        let add = QUANTUM
            .min(free)
            .min(epoch.demand[operator] - grant[operator]);
        grant[operator] += add;
        free -= add;
    }

    Allocation {
        grant,
        unspent: free,
        bids,
    }
}

fn run(trace: &[Epoch], policy: Policy) -> Outcome {
    let mut spill = 0;
    let mut makespan = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut wait = [0_u64; OPERATORS];
    let mut max_wait = 0;
    let mut bids = 0;
    let mut peak = 0;
    let mut unspent = 0;

    for epoch in trace {
        let allocation = allocate(epoch, policy, &wait);
        let used = allocation.grant.iter().sum::<u64>();
        assert!(used <= BUDGET);
        assert!(
            allocation
                .grant
                .iter()
                .zip(epoch.demand)
                .all(|(grant, demand)| *grant <= demand)
        );
        peak = peak.max(used);
        unspent += allocation.unspent;
        bids += allocation.bids;

        let mut epoch_latency = 0;
        for (operator, operator_wait) in wait.iter_mut().enumerate() {
            let grant = allocation.grant[operator];
            let missing = epoch.demand[operator] - grant;
            spill += spill_cost(epoch.demand[operator], grant, epoch.benefit[operator]);
            epoch_latency = epoch_latency.max(100 + missing * 3);
            if grant == BASE_GRANT {
                *operator_wait += 1;
            } else {
                *operator_wait = 0;
            }
            max_wait = max_wait.max(*operator_wait);
            checksum = checksum.rotate_left(5).wrapping_add(
                grant
                    ^ epoch.demand[operator].rotate_left(11)
                    ^ epoch.benefit[operator].rotate_left(23)
                    ^ epoch.token.rotate_left(operator as u32),
            );
        }
        makespan += epoch_latency;
    }

    Outcome {
        checksum,
        makespan,
        spill,
        peak,
        max_wait,
        bids,
        unspent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_policy_uses_the_same_full_budget_without_overgranting() {
        for epoch in trace(Shape::Staggered).into_iter().take(100) {
            for policy in [
                Policy::Fifo,
                Policy::Equal,
                Policy::Priority,
                Policy::Auction,
            ] {
                let allocation = allocate(&epoch, policy, &[0; OPERATORS]);
                assert_eq!(allocation.unspent, 0);
                assert_eq!(allocation.grant.iter().sum::<u64>(), BUDGET);
                for (grant, demand) in allocation.grant.into_iter().zip(epoch.demand) {
                    assert!(grant <= demand);
                }
            }
        }
    }

    #[test]
    fn auction_assigns_the_next_quantum_to_the_largest_exact_saving() {
        let epoch = Epoch {
            demand: [200; OPERATORS],
            benefit: [10, 20, 30, 40, 50, 600],
            token: 0,
        };
        let allocation = allocate(&epoch, Policy::Auction, &[0; OPERATORS]);
        assert_eq!(allocation.grant[5], epoch.demand[5]);
        assert!(allocation.grant[5] > allocation.grant[0]);
    }

    #[test]
    fn accounting_reports_no_abandoned_budget() {
        for policy in [
            Policy::Fifo,
            Policy::Equal,
            Policy::Priority,
            Policy::Auction,
        ] {
            let outcome = run(&trace(Shape::Skewed), policy);
            assert_eq!(outcome.unspent, 0);
            assert_eq!(outcome.peak, BUDGET);
        }
    }
}
