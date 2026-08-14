//! Prototype e60: recycle initialized execution buffers under a hard resident cap.

use common::{Lcg, bench};

const REQUESTS: usize = 2_000;
const RESIDENT_CAP: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Eq, PartialEq)]
enum ShapeKind {
    Scan,
    Join,
    Aggregate,
}
#[derive(Clone, Copy)]
enum Shape {
    Alternating,
    Pressure,
    Churn,
}
#[derive(Clone, Copy)]
enum Policy {
    Fresh,
    SizeClass,
    Lru,
    Selective,
}
#[derive(Clone, Copy)]
struct Request {
    shape: ShapeKind,
    size: usize,
    token: u8,
}
struct Buffer {
    bytes: Vec<u8>,
    shape: ShapeKind,
    logical: usize,
    last: usize,
}
struct Outcome {
    checksum: u64,
    allocated: u64,
    p95: u64,
    slack_pct: f64,
    peak_resident: usize,
    fragment_growth: i64,
}

fn main() {
    println!("e60 — selective buffer recycling (executable prototype, audited)");
    for shape in [Shape::Alternating, Shape::Pressure, Shape::Churn] {
        let trace = fixture(shape);
        let policies = [
            ("fresh allocation", Policy::Fresh),
            ("size-class pool", Policy::SizeClass),
            ("LRU pool", Policy::Lru),
            ("selective recycle", Policy::Selective),
        ];
        let outcomes = policies.map(|(_, policy)| execute(&trace, policy));
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.checksum == outcomes[0].checksum)
        );
        println!("\n=== {} ===", shape_name(shape));
        for ((name, _), out) in policies.iter().zip(outcomes.iter()) {
            println!(
                "{name:<20} allocated {:>10} p95 {:>7} slack {:>5.1}% peak {:>8} frag growth {:+}",
                out.allocated, out.p95, out.slack_pct, out.peak_resident, out.fragment_growth
            );
        }
        for (name, policy) in policies {
            let _ = bench(name, || execute(&trace, policy).checksum);
        }
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Alternating => "alternating shapes",
        Shape::Pressure => "allocator pressure",
        Shape::Churn => "shape churn",
    }
}
fn fixture(shape: Shape) -> Vec<Request> {
    let mut random = Lcg::new(0x6000_0060 ^ shape as u64);
    (0..REQUESTS)
        .map(|index| {
            let kind = match (index + random.below(3) as usize) % 3 {
                0 => ShapeKind::Scan,
                1 => ShapeKind::Join,
                _ => ShapeKind::Aggregate,
            };
            let base = match kind {
                ShapeKind::Scan => 32 * 1024,
                ShapeKind::Join => 128 * 1024,
                ShapeKind::Aggregate => 64 * 1024,
            };
            let multiplier = match shape {
                Shape::Alternating => 1 + index % 2,
                Shape::Pressure => 2 + index % 3,
                Shape::Churn => 1 + random.below(8) as usize,
            };
            Request {
                shape: kind,
                size: base * multiplier,
                token: random.below(251) as u8 + 1,
            }
        })
        .collect()
}

fn candidate(pool: &[Buffer], request: Request, policy: Policy) -> Option<usize> {
    pool.iter()
        .enumerate()
        .filter(|(_, buffer)| buffer.bytes.len() >= request.size)
        .filter(|(_, buffer)| match policy {
            Policy::SizeClass => {
                buffer.bytes.len().next_power_of_two() == request.size.next_power_of_two()
            }
            Policy::Lru => true,
            Policy::Selective => {
                buffer.shape == request.shape && buffer.bytes.len() <= request.size * 2
            }
            Policy::Fresh => false,
        })
        .min_by_key(|(_, buffer)| match policy {
            Policy::Lru => buffer.last,
            _ => buffer.bytes.len() - request.size,
        })
        .map(|(index, _)| index)
}

fn execute(trace: &[Request], policy: Policy) -> Outcome {
    let mut pool = Vec::<Buffer>::new();
    let mut allocated = 0_u64;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut costs = Vec::new();
    let mut peak_resident = 0;
    let mut midpoint_fragment = 0_i64;
    for (tick, request) in trace.iter().copied().enumerate() {
        let selected = candidate(&pool, request, policy);
        let reused = selected.is_some();
        let mut buffer = selected.map_or_else(
            || {
                allocated += request.size as u64;
                Buffer {
                    bytes: vec![0; request.size],
                    shape: request.shape,
                    logical: request.size,
                    last: tick,
                }
            },
            |index| pool.swap_remove(index),
        );
        buffer.bytes[..request.size].fill(0);
        assert!(buffer.bytes[..request.size].iter().all(|byte| *byte == 0));
        buffer.bytes[..request.size].fill(request.token);
        let byte_sum = buffer.bytes[..request.size]
            .iter()
            .map(|byte| u64::from(*byte))
            .sum::<u64>();
        checksum = checksum
            .rotate_left(7)
            .wrapping_add(byte_sum ^ request.size as u64);
        let reshape = u64::from(buffer.shape != request.shape) * request.size as u64 / 32;
        costs.push(
            request.size as u64 / 256 + reshape + if reused { 0 } else { request.size as u64 / 32 },
        );
        buffer.shape = request.shape;
        buffer.logical = request.size;
        buffer.last = tick;
        if !matches!(policy, Policy::Fresh) {
            pool.push(buffer);
            while pool.iter().map(|buffer| buffer.bytes.len()).sum::<usize>() > RESIDENT_CAP {
                let victim = pool
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, buffer)| match policy {
                        Policy::Selective => buffer.bytes.len() - buffer.logical,
                        _ => tick - buffer.last,
                    })
                    .unwrap()
                    .0;
                pool.swap_remove(victim);
            }
        }
        let resident = pool.iter().map(|buffer| buffer.bytes.len()).sum::<usize>();
        peak_resident = peak_resident.max(resident);
        if tick == REQUESTS / 2 {
            midpoint_fragment = pool
                .iter()
                .map(|buffer| (buffer.bytes.len() - buffer.logical) as i64)
                .sum();
        }
    }
    costs.sort_unstable();
    let resident = pool.iter().map(|buffer| buffer.bytes.len()).sum::<usize>();
    let logical = pool.iter().map(|buffer| buffer.logical).sum::<usize>();
    let slack_pct = if resident == 0 {
        0.0
    } else {
        (resident - logical) as f64 * 100.0 / resident as f64
    };
    let final_fragment = pool
        .iter()
        .map(|buffer| (buffer.bytes.len() - buffer.logical) as i64)
        .sum::<i64>();
    Outcome {
        checksum,
        allocated,
        p95: costs[costs.len() * 95 / 100],
        slack_pct,
        peak_resident,
        fragment_growth: final_fragment - midpoint_fragment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_policies_return_the_same_initialized_contents() {
        for shape in [Shape::Alternating, Shape::Pressure, Shape::Churn] {
            let trace = fixture(shape);
            let expected = execute(&trace, Policy::Fresh).checksum;
            for policy in [Policy::SizeClass, Policy::Lru, Policy::Selective] {
                assert_eq!(execute(&trace, policy).checksum, expected);
            }
        }
    }
    #[test]
    fn every_pool_respects_the_resident_cap() {
        let trace = fixture(Shape::Pressure);
        for policy in [Policy::SizeClass, Policy::Lru, Policy::Selective] {
            assert!(execute(&trace, policy).peak_resident <= RESIDENT_CAP);
        }
    }
}
