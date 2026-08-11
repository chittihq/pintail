//! e31: evaporating reinforcement selects join paths under distribution shifts.
//!
//! Evidence tier: deterministic non-stationary policy simulation. Every plan
//! returns the same exact query token; only measured work differs.

use common::{Lcg, bench, check_consistency};

const QUERIES: usize = 36_000;
const PLANS: usize = 4;

#[derive(Clone, Copy)]
enum Shape {
    Stable,
    TwoShifts,
    Noisy,
}

#[derive(Clone, Copy)]
struct Query {
    costs: [u64; PLANS],
    token: u64,
}

#[derive(Clone, Copy)]
enum Policy {
    Static,
    Oracle,
    DiscountedUcb,
    Pheromone,
}

struct Outcome {
    checksum: u64,
    work: u64,
    oracle_work: u64,
    worst_regret_x100: u64,
    recovery: usize,
}

fn main() {
    println!("e31 — ant-pheromone join paths (simulation tier)");
    println!("{QUERIES} queries, {PLANS} exact join paths\n");
    for shape in [Shape::Stable, Shape::TwoShifts, Shape::Noisy] {
        let trace = trace(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("static rule order", Policy::Static),
            ("per-query oracle", Policy::Oracle),
            ("discounted UCB", Policy::DiscountedUcb),
            ("pheromone + evaporation", Policy::Pheromone),
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
                "{name:<25} work {:>10}  vs oracle {:>6.1}%  worst {:>4.2}x  recovery {:>4}",
                outcome.work,
                outcome.work as f64 * 100.0 / outcome.oracle_work as f64,
                outcome.worst_regret_x100 as f64 / 100.0,
                outcome.recovery
            );
        }
        let results = [
            bench("static", || run(&trace, Policy::Static).checksum),
            bench("oracle", || run(&trace, Policy::Oracle).checksum),
            bench("UCB", || run(&trace, Policy::DiscountedUcb).checksum),
            bench("pheromone", || run(&trace, Policy::Pheromone).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Stable => "stable correlations",
        Shape::TwoShifts => "two abrupt distribution shifts",
        Shape::Noisy => "shifts with 15% runtime noise",
    }
}

fn trace(shape: Shape) -> Vec<Query> {
    let mut random = Lcg::new(0xA17C_0031_u64);
    (0..QUERIES)
        .map(|index| {
            let phase = if matches!(shape, Shape::Stable) {
                0
            } else {
                index / (QUERIES / 3)
            };
            let base = match phase {
                0 => [100, 155, 240, 300],
                1 => [290, 100, 170, 235],
                _ => [220, 260, 100, 175],
            };
            let mut costs = base;
            if matches!(shape, Shape::Noisy) {
                for cost in &mut costs {
                    let noise = random.below(31) as i64 - 15;
                    *cost = ((*cost as i64) * (100 + noise) / 100).max(1) as u64;
                }
            }
            Query {
                costs,
                token: random.next_u64(),
            }
        })
        .collect()
}

fn run(trace: &[Query], policy: Policy) -> Outcome {
    let oracle_work = trace
        .iter()
        .map(|query| *query.costs.iter().min().expect("plans"))
        .sum();
    let mut pheromone = [1.0_f64; PLANS];
    let mut cost_ewma = [200.0_f64; PLANS];
    let mut counts = [0.0_f64; PLANS];
    let mut random = Lcg::new(0x5C07_0031_u64);
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut work = 0_u64;
    let mut worst_regret_x100 = 100_u64;
    let mut recovery = 0_usize;
    let mut phase_best = 0_usize;
    let mut shift_at = None;

    for (index, query) in trace.iter().enumerate() {
        let oracle = query
            .costs
            .iter()
            .enumerate()
            .min_by_key(|(_, cost)| **cost)
            .map(|(plan, cost)| (plan, *cost))
            .expect("plans");
        if oracle.0 != phase_best {
            phase_best = oracle.0;
            shift_at = Some(index);
        }
        let plan = match policy {
            Policy::Static => 0,
            Policy::Oracle => oracle.0,
            Policy::DiscountedUcb => {
                if index < PLANS {
                    index
                } else {
                    let total = counts.iter().sum::<f64>().max(1.0);
                    (0..PLANS)
                        .filter(|plan| {
                            cost_ewma[*plan]
                                <= cost_ewma.iter().copied().fold(f64::INFINITY, f64::min) * 2.0
                        })
                        .min_by(|left, right| {
                            let l = cost_ewma[*left]
                                - 35.0 * (total.ln() / counts[*left].max(0.1)).sqrt();
                            let r = cost_ewma[*right]
                                - 35.0 * (total.ln() / counts[*right].max(0.1)).sqrt();
                            l.total_cmp(&r)
                        })
                        .expect("safe arm")
                }
            }
            Policy::Pheromone => {
                let best_cost = cost_ewma.iter().copied().fold(f64::INFINITY, f64::min);
                let safe = (0..PLANS)
                    .filter(|plan| cost_ewma[*plan] <= best_cost * 2.0)
                    .collect::<Vec<_>>();
                if random.below(100) < 4 {
                    safe[random.below(safe.len() as u64) as usize]
                } else {
                    safe.into_iter()
                        .max_by(|left, right| pheromone[*left].total_cmp(&pheromone[*right]))
                        .expect("safe arm")
                }
            }
        };
        let cost = query.costs[plan];
        work += cost;
        worst_regret_x100 = worst_regret_x100.max(cost * 100 / oracle.1);
        checksum = (checksum ^ query.token).wrapping_mul(0x100_0000_01b3);

        for arm in 0..PLANS {
            counts[arm] *= 0.995;
            pheromone[arm] *= 0.985;
        }
        counts[plan] += 1.0;
        cost_ewma[plan] = cost_ewma[plan] * 0.90 + cost as f64 * 0.10;
        pheromone[plan] += 100.0 / cost as f64;
        if let Some(shift) = shift_at
            && plan == phase_best
        {
            recovery = recovery.max(index - shift);
            shift_at = None;
        }
    }
    Outcome {
        checksum,
        work,
        oracle_work,
        worst_regret_x100,
        recovery,
    }
}
