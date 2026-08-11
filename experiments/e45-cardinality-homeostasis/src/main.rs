//! e45: bounded multiplicative cardinality feedback with global gain normalization.
//!
//! Evidence tier: deterministic estimator/plan-choice simulation.

use common::{Lcg, bench, check_consistency};

const QUERIES: usize = 40_000;
const SHAPES: usize = 8;

#[derive(Clone, Copy)]
enum Workload {
    Stable,
    Reversal,
    Noisy,
}

#[derive(Clone, Copy)]
struct Query {
    shape: usize,
    actual: f64,
    static_estimate: f64,
    token: u64,
}

#[derive(Clone, Copy)]
enum Policy {
    Static,
    Unbounded,
    Ewma,
    Homeostatic,
}

struct Outcome {
    checksum: u64,
    median_q100: u64,
    p95_q100: u64,
    execution: u64,
    convergence: usize,
}

fn main() {
    println!("e45 — homeostatic cardinality feedback (simulation tier)");
    println!("{QUERIES} queries, {SHAPES} correlated predicate shapes\n");
    for workload in [Workload::Stable, Workload::Reversal, Workload::Noisy] {
        let trace = trace(workload);
        println!("=== {} ===", workload_name(workload));
        let policies = [
            ("static independence", Policy::Static),
            ("unbounded correction", Policy::Unbounded),
            ("EWMA correction", Policy::Ewma),
            ("homeostatic bounded", Policy::Homeostatic),
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
                "{name:<24} q50 {:>6.2}  q95 {:>7.2}  execution {:>10}  converge {:>3}",
                outcome.median_q100 as f64 / 100.0,
                outcome.p95_q100 as f64 / 100.0,
                outcome.execution,
                outcome.convergence
            );
        }
        let results = [
            bench("static", || run(&trace, Policy::Static).checksum),
            bench("unbounded", || run(&trace, Policy::Unbounded).checksum),
            bench("EWMA", || run(&trace, Policy::Ewma).checksum),
            bench("homeostatic", || run(&trace, Policy::Homeostatic).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn workload_name(workload: Workload) -> &'static str {
    match workload {
        Workload::Stable => "stable correlations",
        Workload::Reversal => "abrupt correlation reversal",
        Workload::Noisy => "reversal with observation noise",
    }
}

fn trace(workload: Workload) -> Vec<Query> {
    let mut random = Lcg::new(0xA0AE_0045_u64);
    (0..QUERIES)
        .map(|index| {
            let shape = random.below(SHAPES as u64) as usize;
            let reversed = !matches!(workload, Workload::Stable) && index >= QUERIES / 2;
            let factor = if reversed {
                1.0 / (1.8 + shape as f64)
            } else {
                1.8 + shape as f64
            };
            let static_estimate = 220.0 + shape as f64 * 90.0;
            let mut actual = static_estimate * factor;
            if matches!(workload, Workload::Noisy) {
                actual *= (85 + random.below(31)) as f64 / 100.0;
            }
            Query {
                shape,
                actual: actual.max(1.0),
                static_estimate,
                token: random.next_u64(),
            }
        })
        .collect()
}

fn run(trace: &[Query], policy: Policy) -> Outcome {
    let mut correction = [1.0_f64; SHAPES];
    let mut qerrors = Vec::with_capacity(trace.len());
    let mut execution = 0_u64;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut convergence = 0_usize;
    let mut shift_seen = false;

    for (index, query) in trace.iter().enumerate() {
        if index == QUERIES / 2 {
            shift_seen = true;
        }
        let estimate = query.static_estimate * correction[query.shape];
        let qerror = (estimate / query.actual).max(query.actual / estimate.max(1.0));
        qerrors.push((qerror * 100.0) as u64);
        execution += plan_cost(estimate, query.actual);
        checksum = (checksum ^ query.token).wrapping_mul(0x100_0000_01b3);

        let ratio = query.actual / query.static_estimate;
        correction[query.shape] = match policy {
            Policy::Static => 1.0,
            Policy::Unbounded => correction[query.shape] * ratio,
            Policy::Ewma => correction[query.shape] * 0.90 + ratio * 0.10,
            Policy::Homeostatic => {
                let local = correction[query.shape] * 0.75 + ratio.clamp(0.125, 8.0) * 0.25;
                local.clamp(0.125, 8.0)
            }
        };
        if matches!(policy, Policy::Homeostatic) {
            let geometric = correction.iter().map(|value| value.ln()).sum::<f64>() / SHAPES as f64;
            let scale = geometric.exp().clamp(0.8, 1.25);
            correction
                .iter_mut()
                .for_each(|value| *value = (*value / scale).clamp(0.125, 8.0));
        }
        if shift_seen && qerror <= 1.5 {
            convergence = index - QUERIES / 2;
            shift_seen = false;
        }
    }
    qerrors.sort_unstable();
    Outcome {
        checksum,
        median_q100: qerrors[qerrors.len() / 2],
        p95_q100: qerrors[qerrors.len() * 95 / 100],
        execution,
        convergence,
    }
}

fn plan_cost(estimate: f64, actual: f64) -> u64 {
    if estimate < 500.0 {
        (80.0 + actual * 1.8) as u64
    } else {
        (260.0 + actual * 0.35) as u64
    }
}
