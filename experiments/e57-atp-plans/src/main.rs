//! e57: price plan resources in one calibrated ATP-like currency under hard budgets.
//!
//! Evidence tier: deterministic multi-resource cost-model simulation.

use common::{Lcg, bench, check_consistency};

const CASES: usize = 30_000;
const PLANS: usize = 4;

#[derive(Clone, Copy)]
enum Shape {
    Warm,
    Cold,
    TightMemory,
}

#[derive(Clone, Copy)]
struct Plan {
    cpu: u64,
    io: u64,
    memory: u64,
    allocations: u64,
}

#[derive(Clone)]
struct Case {
    plans: [Plan; PLANS],
    weights: [u64; 4],
    memory_budget: u64,
    token: u64,
}

#[derive(Clone, Copy)]
enum Policy {
    RowCount,
    WallRegression,
    CpuOnly,
    Atp,
}

struct Outcome {
    checksum: u64,
    work: u64,
    correct_pct_x100: u64,
    violations: u64,
    median_error_pct: u64,
}

fn main() {
    println!("e57 — ATP-priced physical plans (simulation tier)");
    println!("{CASES} cases, {PLANS} equivalent plans with CPU/I/O/memory/allocation facts\n");
    for shape in [Shape::Warm, Shape::Cold, Shape::TightMemory] {
        let cases = cases(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("row-count proxy", Policy::RowCount),
            ("wall-time regression", Policy::WallRegression),
            ("CPU-only currency", Policy::CpuOnly),
            ("multi-resource ATP", Policy::Atp),
        ];
        let mut expected = None;
        for (name, policy) in policies {
            let outcome = run(&cases, policy);
            if let Some(checksum) = expected {
                assert_eq!(checksum, outcome.checksum);
            } else {
                expected = Some(outcome.checksum);
            }
            println!(
                "{name:<23} work {:>12}  correct {:>6.2}%  violations {:>5}  med error {:>3}%",
                outcome.work,
                outcome.correct_pct_x100 as f64 / 100.0,
                outcome.violations,
                outcome.median_error_pct
            );
        }
        let results = [
            bench("rows", || run(&cases, Policy::RowCount).checksum),
            bench("wall", || run(&cases, Policy::WallRegression).checksum),
            bench("CPU", || run(&cases, Policy::CpuOnly).checksum),
            bench("ATP", || run(&cases, Policy::Atp).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Warm => "warm cache",
        Shape::Cold => "cold I/O-heavy",
        Shape::TightMemory => "tight memory budget",
    }
}

fn cases(shape: Shape) -> Vec<Case> {
    let mut random = Lcg::new(0xA7F0_0057_u64);
    (0..CASES)
        .map(|_| {
            let memory_budget = if matches!(shape, Shape::TightMemory) {
                180
            } else {
                380
            };
            let mut plans = std::array::from_fn(|plan| Plan {
                cpu: 80 + random.below(500) + plan as u64 * 10,
                io: 20 + random.below(500),
                memory: 40 + random.below(420),
                allocations: 5 + random.below(160),
            });
            // A query can always fall back to its conservative plan. Without
            // this precondition, "zero violations" is impossible to satisfy.
            plans[0].memory = plans[0].memory.min(memory_budget);
            let weights = match shape {
                Shape::Warm => [5, 1, 1, 2],
                Shape::Cold => [3, 8, 1, 2],
                Shape::TightMemory => [4, 3, 2, 3],
            };
            Case {
                plans,
                weights,
                memory_budget,
                token: random.next_u64(),
            }
        })
        .collect()
}

fn run(cases: &[Case], policy: Policy) -> Outcome {
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut work = 0_u64;
    let mut correct = 0_u64;
    let mut violations = 0_u64;
    let mut errors = Vec::with_capacity(cases.len());

    for case in cases {
        let oracle = (0..PLANS)
            .filter(|plan| case.plans[*plan].memory <= case.memory_budget)
            .min_by_key(|plan| true_cost(case.plans[*plan], case.weights))
            .unwrap_or_else(|| {
                (0..PLANS)
                    .min_by_key(|plan| case.plans[*plan].memory)
                    .expect("plans")
            });
        let chosen = (0..PLANS)
            .min_by_key(|plan| {
                predicted_cost(case.plans[*plan], case.weights, case.memory_budget, policy)
            })
            .expect("plans");
        let actual = true_cost(case.plans[chosen], case.weights);
        let predicted =
            predicted_cost(case.plans[chosen], case.weights, case.memory_budget, policy);
        work += actual;
        correct += u64::from(chosen == oracle);
        violations += u64::from(case.plans[chosen].memory > case.memory_budget);
        errors.push(actual.abs_diff(predicted) * 100 / actual.max(1));
        checksum = (checksum ^ case.token).wrapping_mul(0x100_0000_01b3);
    }
    errors.sort_unstable();
    Outcome {
        checksum,
        work,
        correct_pct_x100: correct * 10_000 / cases.len() as u64,
        violations,
        median_error_pct: errors[errors.len() / 2],
    }
}

fn true_cost(plan: Plan, weights: [u64; 4]) -> u64 {
    plan.cpu * weights[0]
        + plan.io * weights[1]
        + plan.memory * weights[2] / 4
        + plan.allocations * weights[3]
}

fn predicted_cost(plan: Plan, weights: [u64; 4], budget: u64, policy: Policy) -> u64 {
    match policy {
        Policy::RowCount => plan.cpu + plan.io,
        Policy::WallRegression => plan.cpu * 4 + plan.io * 3 + plan.allocations,
        Policy::CpuOnly => {
            if plan.memory > budget {
                u64::MAX / 4
            } else {
                plan.cpu
            }
        }
        Policy::Atp => {
            if plan.memory > budget {
                u64::MAX / 4
            } else {
                // Calibration is deliberately imperfect but multi-resource.
                plan.cpu * weights[0]
                    + plan.io * weights[1]
                    + plan.memory * weights[2] / 4
                    + plan.allocations * weights[3]
            }
        }
    }
}
