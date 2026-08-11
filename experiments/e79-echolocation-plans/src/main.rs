//! e79: issue a tiny stratified probe only when optimizer uncertainty can repay it.
//!
//! Evidence tier: deterministic sampling/re-optimization simulation.

use common::{Lcg, bench, check_consistency};

const QUERIES: usize = 40_000;
const PLANS: usize = 3;

#[derive(Clone, Copy)]
enum Shape {
    Correlated,
    Accurate,
    Small,
    Mixed,
}

#[derive(Clone, Copy)]
struct Query {
    actual_selectivity: f64,
    estimate: f64,
    uncertainty: f64,
    scale: u64,
    token: u64,
}

#[derive(Clone, Copy)]
enum Policy {
    Static,
    AlwaysProbe,
    Triggered,
    Oracle,
}

struct Outcome {
    checksum: u64,
    work: u64,
    probes: u64,
    wrong_plans: u64,
}

fn main() {
    println!("e79 — bat-echolocation probe plans (simulation tier)");
    println!("{QUERIES} queries, {PLANS} exact alternatives\n");
    for shape in [
        Shape::Correlated,
        Shape::Accurate,
        Shape::Small,
        Shape::Mixed,
    ] {
        let trace = trace(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("static statistics", Policy::Static),
            ("always sample", Policy::AlwaysProbe),
            ("uncertainty-triggered echo", Policy::Triggered),
            ("per-query oracle", Policy::Oracle),
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
                "{name:<27} work {:>11}  probes {:>6}  wrong plans {:>6}",
                outcome.work, outcome.probes, outcome.wrong_plans
            );
        }
        let results = [
            bench("static", || run(&trace, Policy::Static).checksum),
            bench("always", || run(&trace, Policy::AlwaysProbe).checksum),
            bench("triggered", || run(&trace, Policy::Triggered).checksum),
            bench("oracle", || run(&trace, Policy::Oracle).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Correlated => "stale correlated estimates",
        Shape::Accurate => "accurate-statistics control",
        Shape::Small => "small queries where probes cannot repay",
        Shape::Mixed => "mixed certainty and scale",
    }
}

fn trace(shape: Shape) -> Vec<Query> {
    let mut random = Lcg::new(0xBA70_0079_u64);
    (0..QUERIES)
        .map(|index| {
            let actual = (1 + random.below(900)) as f64 / 1_000.0;
            let uncertain = matches!(shape, Shape::Correlated)
                || (matches!(shape, Shape::Mixed) && index.is_multiple_of(2));
            let estimate = if uncertain {
                (actual * if index.is_multiple_of(3) { 0.12 } else { 4.5 }).clamp(0.001, 0.999)
            } else {
                (actual * (95 + random.below(11)) as f64 / 100.0).clamp(0.001, 0.999)
            };
            Query {
                actual_selectivity: actual,
                estimate,
                uncertainty: if uncertain { 0.9 } else { 0.05 },
                scale: if matches!(shape, Shape::Small) { 1 } else { 10 },
                token: random.next_u64(),
            }
        })
        .collect()
}

fn run(trace: &[Query], policy: Policy) -> Outcome {
    let mut random = Lcg::new(0xEC40_0079_u64);
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut work = 0_u64;
    let mut probes = 0_u64;
    let mut wrong_plans = 0_u64;

    for query in trace {
        let oracle = best_plan(query.actual_selectivity, query.scale);
        let static_plan = best_plan(query.estimate, query.scale);
        let probe_cost = 55 * query.scale;
        let static_regret_bound = (query.uncertainty * 1_200.0 * query.scale as f64) as u64;
        let probe = match policy {
            Policy::AlwaysProbe => true,
            Policy::Triggered => query.uncertainty > 0.25 && static_regret_bound > probe_cost * 2,
            _ => false,
        };
        let plan = match policy {
            Policy::Oracle => oracle,
            _ if probe => {
                probes += 1;
                work += probe_cost;
                let noisy = (query.actual_selectivity * (97 + random.below(7)) as f64 / 100.0)
                    .clamp(0.001, 0.999);
                best_plan(noisy, query.scale)
            }
            _ => static_plan,
        };
        work += plan_cost(plan, query.actual_selectivity, query.scale);
        wrong_plans += u64::from(plan != oracle);
        checksum = (checksum ^ query.token).wrapping_mul(0x100_0000_01b3);
    }
    Outcome {
        checksum,
        work,
        probes,
        wrong_plans,
    }
}

fn best_plan(selectivity: f64, scale: u64) -> usize {
    (0..PLANS)
        .min_by_key(|plan| plan_cost(*plan, selectivity, scale))
        .expect("plans")
}

fn plan_cost(plan: usize, selectivity: f64, scale: u64) -> u64 {
    let base = match plan {
        0 => 1_000.0,
        1 => 75.0 + selectivity * 8_000.0,
        2 => 420.0 + selectivity * 1_600.0,
        _ => unreachable!(),
    };
    (base * scale as f64) as u64
}
