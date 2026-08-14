//! Prototype e49: allocate a hard runnable-task budget among concurrent queries.

use common::{Lcg, bench};

const SLOTS: u32 = 16;
const TENANTS: usize = 8;

#[derive(Clone, Copy)]
enum Shape {
    Mixed,
    MemoryCpu,
    Burst,
}
#[derive(Clone, Copy)]
enum Policy {
    Fixed,
    Equal,
    Sjf,
    School,
}

#[derive(Clone)]
struct Query {
    arrival: u64,
    work: u64,
    max_parallel: u32,
    tenant: usize,
    token: u64,
}

struct Outcome {
    checksum: u64,
    p95_slowdown: f64,
    throughput: f64,
    fairness: f64,
    max_wait: u64,
}

fn main() {
    println!("e49 — schooling concurrency control (executable prototype, audited)");
    for shape in [Shape::Mixed, Shape::MemoryCpu, Shape::Burst] {
        let queries = fixture(shape);
        let policies = [
            ("fixed pool", Policy::Fixed),
            ("equal slots", Policy::Equal),
            ("shortest job", Policy::Sjf),
            ("schooling", Policy::School),
        ];
        let outcomes = policies.map(|(_, policy)| execute(&queries, policy));
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.checksum == outcomes[0].checksum)
        );
        println!("\n=== {} ===", shape_name(shape));
        for ((name, _), outcome) in policies.iter().zip(outcomes.iter()) {
            println!(
                "{name:<16} p95 slowdown {:>6.2} throughput {:>6.2} fairness {:.3} max wait {}",
                outcome.p95_slowdown, outcome.throughput, outcome.fairness, outcome.max_wait
            );
        }
        for (name, policy) in policies {
            let _ = bench(name, || execute(&queries, policy).checksum);
        }
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Mixed => "mixed short/long",
        Shape::MemoryCpu => "memory + CPU",
        Shape::Burst => "arrival burst",
    }
}

fn fixture(shape: Shape) -> Vec<Query> {
    let mut random = Lcg::new(0x4900_0049 ^ shape as u64);
    (0..160)
        .map(|index| {
            let long = index % 5 == 0;
            Query {
                arrival: match shape {
                    Shape::Burst => (index / 40 * 80) as u64,
                    _ => (index * 3) as u64,
                },
                work: if long {
                    500 + random.below(300)
                } else {
                    30 + random.below(70)
                },
                max_parallel: match shape {
                    Shape::MemoryCpu if index % 2 == 0 => 2,
                    _ if long => 8,
                    _ => 3,
                },
                tenant: index % TENANTS,
                token: random.next_u64(),
            }
        })
        .collect()
}

fn allocate(
    policy: Policy,
    queries: &[Query],
    remaining: &[u64],
    started: &[Option<u64>],
    tick: u64,
) -> Vec<u32> {
    let active = (0..queries.len())
        .filter(|index| queries[*index].arrival <= tick && remaining[*index] > 0)
        .collect::<Vec<_>>();
    let mut grants = vec![0_u32; queries.len()];
    let mut free = SLOTS;
    if active.is_empty() {
        return grants;
    }
    match policy {
        Policy::Sjf => {
            let mut order = active;
            order.sort_unstable_by_key(|index| (remaining[*index], queries[*index].arrival));
            for index in order {
                let add = free.min(queries[index].max_parallel);
                grants[index] = add;
                free -= add;
                if free == 0 {
                    break;
                }
            }
        }
        Policy::Fixed => {
            for index in active {
                let add = free.min(4).min(queries[index].max_parallel);
                grants[index] = add;
                free -= add;
                if free == 0 {
                    break;
                }
            }
        }
        Policy::Equal | Policy::School => {
            let mut order = active;
            if matches!(policy, Policy::School) {
                order.sort_unstable_by_key(|index| {
                    let age = tick.saturating_sub(queries[*index].arrival);
                    std::cmp::Reverse((
                        age / 20,
                        queries[*index].max_parallel,
                        std::cmp::Reverse(remaining[*index]),
                    ))
                });
            }
            while free > 0 {
                let mut changed = false;
                for &index in &order {
                    if free == 0 {
                        break;
                    }
                    if grants[index] < queries[index].max_parallel {
                        grants[index] += 1;
                        free -= 1;
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
        }
    }
    let _ = started;
    grants
}

fn execute(queries: &[Query], policy: Policy) -> Outcome {
    let mut remaining = queries.iter().map(|query| query.work).collect::<Vec<_>>();
    let mut started = vec![None; queries.len()];
    let mut finished = vec![None; queries.len()];
    let mut tick = 0_u64;
    let mut max_wait = 0_u64;
    while finished.iter().any(Option::is_none) {
        let grants = allocate(policy, queries, &remaining, &started, tick);
        if grants.iter().all(|grant| *grant == 0) {
            tick += 1;
            continue;
        }
        for index in 0..queries.len() {
            if grants[index] == 0 {
                continue;
            }
            started[index].get_or_insert(tick);
            remaining[index] = remaining[index].saturating_sub(u64::from(grants[index]));
            if remaining[index] == 0 && finished[index].is_none() {
                finished[index] = Some(tick + 1);
            }
        }
        for index in 0..queries.len() {
            if queries[index].arrival <= tick && remaining[index] > 0 && started[index].is_none() {
                max_wait = max_wait.max(tick - queries[index].arrival);
            }
        }
        tick += 1;
    }
    let mut slowdowns = Vec::new();
    let mut tenant_rates = [0_f64; TENANTS];
    let mut checksum = 0_u64;
    for (index, query) in queries.iter().enumerate() {
        let latency = finished[index].unwrap() - query.arrival;
        let ideal = query.work.div_ceil(u64::from(query.max_parallel)).max(1);
        let slowdown = latency as f64 / ideal as f64;
        slowdowns.push(slowdown);
        tenant_rates[query.tenant] += 1.0 / slowdown;
        checksum ^= query.token.rotate_left((index % 63) as u32);
    }
    slowdowns.sort_by(|a, b| a.total_cmp(b));
    let sum = tenant_rates.iter().sum::<f64>();
    let fairness =
        sum * sum / (TENANTS as f64 * tenant_rates.iter().map(|rate| rate * rate).sum::<f64>());
    Outcome {
        checksum,
        p95_slowdown: slowdowns[slowdowns.len() * 95 / 100],
        throughput: queries.iter().map(|query| query.work).sum::<u64>() as f64 / tick as f64,
        fairness,
        max_wait,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_scheduler_completes_the_exact_workload() {
        let queries = fixture(Shape::Mixed);
        let expected = execute(&queries, Policy::Fixed).checksum;
        for policy in [Policy::Equal, Policy::Sjf, Policy::School] {
            assert_eq!(execute(&queries, policy).checksum, expected);
        }
    }
    #[test]
    fn school_completes_every_query_and_respects_fairness_floor() {
        for shape in [Shape::Mixed, Shape::MemoryCpu, Shape::Burst] {
            let result = execute(&fixture(shape), Policy::School);
            assert!(result.fairness >= 0.9);
        }
    }
}
