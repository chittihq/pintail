//! e77: divide an immutable segment into variable-width metadata granules.
//!
//! Evidence tier: deterministic layout simulation. Every query's result is an
//! arithmetic exact range sum; policies differ only in rows decoded around it.

use common::{Lcg, bench, check_consistency};

const ROWS: u64 = 1 << 20;
const CELLS: usize = 1024;
const GRANULES: usize = 64;
const CELL_ROWS: u64 = ROWS / CELLS as u64;
const PHASE_QUERIES: usize = 5_000;
const EPOCH: usize = 250;

#[derive(Clone, Copy)]
enum Shape {
    Hot,
    Moving,
    UniformScan,
    HighEntropy,
}

#[derive(Clone, Copy)]
enum Policy {
    Fixed,
    Entropy,
    Heat,
    Foveated,
}

#[derive(Clone, Copy)]
struct Query {
    start: u64,
    end: u64,
    update_overlap: f64,
}

struct Outcome {
    checksum: u64,
    decoded: u64,
    p95: u64,
    boundary_changes: u64,
}

fn main() {
    println!("e77 — retinal variable-resolution granules (simulation tier)");
    println!("{ROWS} rows, {GRANULES} granules for every policy, {CELL_ROWS}-row layout cells\n");
    for shape in [
        Shape::Hot,
        Shape::Moving,
        Shape::UniformScan,
        Shape::HighEntropy,
    ] {
        let trace = make_trace(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("fixed 16K", Policy::Fixed),
            ("static entropy", Policy::Entropy),
            ("query heat only", Policy::Heat),
            ("bounded heat + overlap fovea", Policy::Foveated),
        ];
        let mut checksum = None;
        for (name, policy) in policies {
            let outcome = run(&trace, policy);
            if let Some(expected) = checksum {
                assert_eq!(expected, outcome.checksum);
            } else {
                checksum = Some(outcome.checksum);
            }
            println!(
                "{name:<31} decoded {:>13}  p95 {:>8}  boundary changes {:>6}",
                outcome.decoded, outcome.p95, outcome.boundary_changes
            );
        }
        let results = [
            bench("fixed", || run(&trace, Policy::Fixed).checksum),
            bench("entropy", || run(&trace, Policy::Entropy).checksum),
            bench("heat", || run(&trace, Policy::Heat).checksum),
            bench("foveated", || run(&trace, Policy::Foveated).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Hot => "stationary hot fovea",
        Shape::Moving => "moving hotspot",
        Shape::UniformScan => "uniform wide-scan hostile control",
        Shape::HighEntropy => "random narrow probes",
    }
}

fn make_trace(shape: Shape) -> Vec<Query> {
    let phases = if matches!(shape, Shape::Moving) { 4 } else { 1 };
    let mut random = Lcg::new(0xAE71_0077_u64);
    let mut trace = Vec::with_capacity(phases * PHASE_QUERIES);
    for phase in 0..phases {
        let hot_cell = match shape {
            Shape::Moving => (170 + phase * 211) % (CELLS - 80),
            _ => 360,
        };
        for _ in 0..PHASE_QUERIES {
            let (start, width, overlap) = match shape {
                Shape::Hot | Shape::Moving => {
                    if random.below(100) < 85 {
                        let cell = hot_cell + random.below(64) as usize;
                        (cell as u64 * CELL_ROWS + random.below(CELL_ROWS), 192, 0.8)
                    } else {
                        (random.below(ROWS - 256), 192, 0.05)
                    }
                }
                Shape::UniformScan => (random.below(ROWS - 65_536), 65_536, 0.1),
                Shape::HighEntropy => (
                    random.below(ROWS - 256),
                    192,
                    random.below(100) as f64 / 100.0,
                ),
            };
            trace.push(Query {
                start,
                end: (start + width).min(ROWS - 1),
                update_overlap: overlap,
            });
        }
    }
    trace
}

fn run(trace: &[Query], policy: Policy) -> Outcome {
    let entropy = entropy_weights();
    let mut heat = vec![1.0_f64; CELLS];
    let mut overlap = vec![0.0_f64; CELLS];
    let mut boundaries = match policy {
        Policy::Fixed | Policy::Heat | Policy::Foveated => fixed_boundaries(),
        Policy::Entropy => weighted_boundaries(&entropy, false),
    };
    let mut decoded_each = Vec::with_capacity(trace.len());
    let mut decoded = 0_u64;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut boundary_changes = 0_u64;

    for (index, query) in trace.iter().enumerate() {
        let query_decoded = decoded_rows(&boundaries, *query);
        decoded += query_decoded;
        decoded_each.push(query_decoded);
        let exact = range_sum(query.start, query.end);
        checksum = (checksum ^ exact).wrapping_mul(0x100_0000_01b3);

        let first = (query.start / CELL_ROWS) as usize;
        let last = (query.end / CELL_ROWS) as usize;
        for cell in first..=last.min(CELLS - 1) {
            heat[cell] += 1.0;
            overlap[cell] += query.update_overlap;
        }

        if (index + 1) % EPOCH == 0 && matches!(policy, Policy::Heat | Policy::Foveated) {
            let weights = match policy {
                Policy::Heat => heat.clone(),
                Policy::Foveated => heat
                    .iter()
                    .zip(&overlap)
                    .zip(&entropy)
                    .map(|((heat, overlap), entropy)| {
                        1.0 + heat.sqrt() + 2.0 * overlap.sqrt() + 0.15 * entropy
                    })
                    .collect(),
                _ => unreachable!(),
            };
            let next = weighted_boundaries(&weights, matches!(policy, Policy::Foveated));
            boundary_changes += boundaries
                .iter()
                .zip(&next)
                .filter(|(left, right)| left != right)
                .count() as u64;
            boundaries = next;
            heat.iter_mut()
                .for_each(|value| *value = 1.0 + (*value - 1.0) * 0.72);
            overlap.iter_mut().for_each(|value| *value *= 0.72);
        }
    }

    decoded_each.sort_unstable();
    Outcome {
        checksum,
        decoded,
        p95: decoded_each[decoded_each.len() * 95 / 100],
        boundary_changes,
    }
}

fn fixed_boundaries() -> Vec<usize> {
    (0..=GRANULES)
        .map(|index| index * CELLS / GRANULES)
        .collect()
}

fn entropy_weights() -> Vec<f64> {
    (0..CELLS)
        .map(|cell| {
            let clustered_surprise = if (340..440).contains(&cell) { 8.0 } else { 1.0 };
            clustered_surprise + (cell % 17) as f64 / 50.0
        })
        .collect()
}

fn weighted_boundaries(weights: &[f64], bounded: bool) -> Vec<usize> {
    let mut prefix = Vec::with_capacity(weights.len() + 1);
    prefix.push(0.0);
    for weight in weights {
        prefix.push(prefix.last().copied().expect("prefix starts at zero") + weight);
    }
    let total = *prefix.last().expect("prefix contains total");
    let mut boundaries = vec![0];
    for granule in 1..GRANULES {
        let target = total * granule as f64 / GRANULES as f64;
        let minimum = boundaries.last().copied().expect("first boundary") + 1;
        let maximum = CELLS - (GRANULES - granule);
        let mut next = prefix
            .partition_point(|value| *value < target)
            .clamp(minimum, maximum);
        if bounded {
            let fixed = granule * CELLS / GRANULES;
            next = next.clamp(
                fixed.saturating_sub(48).max(minimum),
                (fixed + 48).min(maximum),
            );
        }
        boundaries.push(next);
    }
    boundaries.push(CELLS);
    boundaries
}

fn decoded_rows(boundaries: &[usize], query: Query) -> u64 {
    let first_cell = (query.start / CELL_ROWS) as usize;
    let last_cell = (query.end / CELL_ROWS) as usize;
    boundaries
        .windows(2)
        .filter(|window| window[1] > first_cell && window[0] <= last_cell)
        .map(|window| (window[1] - window[0]) as u64 * CELL_ROWS)
        .sum()
}

fn range_sum(start: u64, end: u64) -> u64 {
    let count = end - start + 1;
    count.wrapping_mul(start.wrapping_add(end)) / 2
}
