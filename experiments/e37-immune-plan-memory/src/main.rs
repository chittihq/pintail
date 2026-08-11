//! e37: recall a plan only inside the parameter/data-shape envelope where it won.
//!
//! Evidence tier: parameter-sensitive plan policy simulation with concept drift.

use common::{Lcg, bench, check_consistency};

const QUERIES: usize = 50_000;
const PLANS: usize = 3;
const BUCKETS: usize = 10;

#[derive(Clone, Copy)]
enum Shape {
    Stable,
    Shift,
    Skew,
}

#[derive(Clone, Copy)]
struct Query {
    parameter: u8,
    costs: [u64; PLANS],
    token: u64,
}

#[derive(Clone, Copy)]
enum Policy {
    OnePlan,
    TextLru,
    Buckets,
    Affinity,
}

struct Outcome {
    checksum: u64,
    work: u64,
    oracle: u64,
    planning: u64,
    worst_recovery: usize,
}

fn main() {
    println!("e37 — immune-memory plan recall (simulation tier)");
    println!("{QUERIES} parameter-sensitive executions, {PLANS} exact plans\n");
    for shape in [Shape::Stable, Shape::Shift, Shape::Skew] {
        let trace = trace(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("one cached plan", Policy::OnePlan),
            ("SQL-text LRU", Policy::TextLru),
            ("fixed parameter buckets", Policy::Buckets),
            ("affinity + decay memory", Policy::Affinity),
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
                "{name:<25} work {:>11}  vs oracle {:>6.1}%  planning {:>7}  recovery {:>3}",
                outcome.work,
                outcome.work as f64 * 100.0 / outcome.oracle as f64,
                outcome.planning,
                outcome.worst_recovery
            );
        }
        let results = [
            bench("one", || run(&trace, Policy::OnePlan).checksum),
            bench("text LRU", || run(&trace, Policy::TextLru).checksum),
            bench("buckets", || run(&trace, Policy::Buckets).checksum),
            bench("affinity", || run(&trace, Policy::Affinity).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Stable => "stable parameter envelopes",
        Shape::Shift => "boundary and winner shift",
        Shape::Skew => "hot parameters plus shift",
    }
}

fn trace(shape: Shape) -> Vec<Query> {
    let mut random = Lcg::new(0x1AAE_0037_u64);
    (0..QUERIES)
        .map(|index| {
            let parameter = if matches!(shape, Shape::Skew) && random.below(100) < 80 {
                (40 + random.below(20)) as u8
            } else {
                random.below(100) as u8
            };
            let shifted = !matches!(shape, Shape::Stable) && index >= QUERIES / 2;
            let winner = if shifted {
                match parameter {
                    0..=19 => 2,
                    20..=64 => 0,
                    _ => 1,
                }
            } else {
                usize::from(parameter >= 33) + usize::from(parameter >= 66)
            };
            let mut costs = [260_u64, 260, 260];
            costs[winner] = 100;
            costs[(winner + 1) % PLANS] = 165;
            Query {
                parameter,
                costs,
                token: random.next_u64(),
            }
        })
        .collect()
}

fn run(trace: &[Query], policy: Policy) -> Outcome {
    let oracle = trace
        .iter()
        .map(|query| *query.costs.iter().min().expect("plans"))
        .sum::<u64>();
    let mut global = [180.0_f64; PLANS];
    let mut bucket = [[[180.0_f64; PLANS]; BUCKETS]; 2];
    let mut seen = [[[0_u64; PLANS]; BUCKETS]; 2];
    let mut global_seen = [0_u64; PLANS];
    let mut random = Lcg::new(0x5C07_0037_u64);
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut work = 0_u64;
    let mut planning = 0_u64;
    let mut shift_recovery = None;
    let mut worst_recovery = 0_usize;

    for (index, query) in trace.iter().enumerate() {
        let phase = usize::from(index >= QUERIES / 2);
        if index == QUERIES / 2 {
            shift_recovery = Some(index);
        }
        let bucket_id = usize::from(query.parameter) / (100 / BUCKETS);
        let plan = match policy {
            Policy::OnePlan => 0,
            Policy::TextLru => {
                planning += 1;
                if let Some(unseen) = global_seen.iter().position(|count| *count == 0) {
                    unseen
                } else if random.below(100) < 2 {
                    random.below(PLANS as u64) as usize
                } else {
                    argmin(&global)
                }
            }
            Policy::Buckets | Policy::Affinity => {
                planning += 2;
                if let Some(unseen) = seen[phase][bucket_id].iter().position(|count| *count == 0) {
                    unseen
                } else if matches!(policy, Policy::Affinity) && random.below(100) < 3 {
                    random.below(PLANS as u64) as usize
                } else {
                    argmin(&bucket[phase][bucket_id])
                }
            }
        };
        work += query.costs[plan];
        checksum = (checksum ^ query.token).wrapping_mul(0x100_0000_01b3);
        global_seen[plan] += 1;
        global[plan] = global[plan] * 0.98 + query.costs[plan] as f64 * 0.02;
        seen[phase][bucket_id][plan] += 1;
        let alpha = if matches!(policy, Policy::Affinity) {
            0.15
        } else {
            1.0 / seen[phase][bucket_id][plan].min(200) as f64
        };
        bucket[phase][bucket_id][plan] =
            bucket[phase][bucket_id][plan] * (1.0 - alpha) + query.costs[plan] as f64 * alpha;
        let oracle_plan = argmin_u64(&query.costs);
        if let Some(start) = shift_recovery
            && plan == oracle_plan
        {
            worst_recovery = index - start;
            shift_recovery = None;
        }
    }
    Outcome {
        checksum,
        work: work + planning,
        oracle,
        planning,
        worst_recovery,
    }
}

fn argmin(values: &[f64; PLANS]) -> usize {
    values
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(index, _)| index)
        .expect("plans")
}

fn argmin_u64(values: &[u64; PLANS]) -> usize {
    values
        .iter()
        .enumerate()
        .min_by_key(|(_, value)| **value)
        .map(|(index, _)| index)
        .expect("plans")
}
