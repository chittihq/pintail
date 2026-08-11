//! e32: remodel a fixed sparse-index byte budget toward persistent seek stress.
//!
//! Evidence tier: deterministic policy simulation. The simulated lookup always
//! returns the same value; only the number of rows between adjacent pivots changes.

use common::{Lcg, bench, check_consistency};

const ROWS: u64 = 1 << 24;
const REGIONS: usize = 256;
const PIVOTS: u32 = 4096;
const QUERIES_PER_PHASE: usize = 20_000;
const EPOCH: usize = 200;

#[derive(Clone, Copy)]
enum Shape {
    Uniform,
    Zipf,
    Moving,
    ScanHeavy,
}

#[derive(Clone, Copy)]
enum Policy {
    Fixed,
    Frequency,
    Remodeling,
}

struct Outcome {
    checksum: u64,
    decoded: u64,
    p95: u64,
    changes: u64,
}

fn main() {
    println!("e32 — bone-remodelled sparse indexes (simulation tier)");
    println!("{ROWS} rows, {REGIONS} regions, {PIVOTS} pivots, equal metadata bytes\n");

    for shape in [Shape::Uniform, Shape::Zipf, Shape::Moving, Shape::ScanHeavy] {
        let trace = make_trace(shape);
        println!("=== {} ===", shape_name(shape));
        let outcomes = [
            ("fixed 16K-equivalent", run(&trace, Policy::Fixed)),
            ("frequency allocation", run(&trace, Policy::Frequency)),
            (
                "stress + decay + hysteresis",
                run(&trace, Policy::Remodeling),
            ),
        ];
        for (name, value) in &outcomes {
            println!(
                "{name:<30} decoded {:>12}  p95 {:>7} rows  pivot changes {:>7}",
                value.decoded, value.p95, value.changes
            );
        }
        assert!(
            outcomes
                .iter()
                .all(|(_, value)| value.checksum == outcomes[0].1.checksum)
        );

        let results = [
            bench("fixed", || run(&trace, Policy::Fixed).checksum),
            bench("frequency", || run(&trace, Policy::Frequency).checksum),
            bench("remodeling", || run(&trace, Policy::Remodeling).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Uniform => "uniform points",
        Shape::Zipf => "stationary 80/20 hotspot",
        Shape::Moving => "moving hotspot",
        Shape::ScanHeavy => "90% points + 10% full scans",
    }
}

fn make_trace(shape: Shape) -> Vec<Option<u64>> {
    let phases = if matches!(shape, Shape::Moving) { 4 } else { 1 };
    let mut random = Lcg::new(0xB00E_0032_u64);
    let mut trace = Vec::with_capacity(QUERIES_PER_PHASE * phases);
    for phase in 0..phases {
        let hot_start = ((phase * 61) % (REGIONS - 32)) as u64;
        for _ in 0..QUERIES_PER_PHASE {
            if matches!(shape, Shape::ScanHeavy) && random.below(10) == 0 {
                trace.push(None);
                continue;
            }
            let region = match shape {
                Shape::Uniform | Shape::ScanHeavy => random.below(REGIONS as u64),
                Shape::Zipf | Shape::Moving => {
                    if random.below(10) < 8 {
                        hot_start + random.below(32)
                    } else {
                        random.below(REGIONS as u64)
                    }
                }
            };
            let region_rows = ROWS / REGIONS as u64;
            trace.push(Some(region * region_rows + random.below(region_rows)));
        }
    }
    trace
}

fn run(trace: &[Option<u64>], policy: Policy) -> Outcome {
    let mut pivots = vec![PIVOTS / REGIONS as u32; REGIONS];
    let mut stress = vec![0.0_f64; REGIONS];
    let mut decoded_each = Vec::with_capacity(trace.len());
    let mut decoded = 0_u64;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut changes = 0_u64;
    let region_rows = ROWS / REGIONS as u64;

    for (index, query) in trace.iter().enumerate() {
        match query {
            Some(key) => {
                let region = (*key / region_rows) as usize;
                let work = region_rows.div_ceil(u64::from(pivots[region]));
                decoded = decoded.wrapping_add(work);
                decoded_each.push(work);
                stress[region] += work as f64;
                let value = key.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(17);
                checksum = (checksum ^ value).wrapping_mul(0x100_0000_01b3);
            }
            None => {
                decoded = decoded.wrapping_add(ROWS);
                decoded_each.push(ROWS);
                checksum = (checksum ^ ROWS).wrapping_mul(0x100_0000_01b3);
            }
        }

        if (index + 1) % EPOCH == 0 && !matches!(policy, Policy::Fixed) {
            let next = allocate(&stress, &pivots, policy);
            changes += pivots
                .iter()
                .zip(&next)
                .map(|(old, new)| u64::from(old.abs_diff(*new)))
                .sum::<u64>();
            pivots = next;
            if matches!(policy, Policy::Remodeling) {
                stress.iter_mut().for_each(|value| *value *= 0.82);
            }
        }
    }

    decoded_each.sort_unstable();
    Outcome {
        checksum,
        decoded,
        p95: decoded_each[decoded_each.len() * 95 / 100],
        changes,
    }
}

fn allocate(stress: &[f64], current: &[u32], policy: Policy) -> Vec<u32> {
    let floor = 2_u32;
    let remaining = PIVOTS - floor * REGIONS as u32;
    let weights = stress
        .iter()
        .map(|value| match policy {
            Policy::Frequency => *value,
            Policy::Remodeling => value.sqrt(),
            Policy::Fixed => unreachable!(),
        })
        .collect::<Vec<_>>();
    let total = weights.iter().sum::<f64>().max(f64::EPSILON);
    let mut target = weights
        .iter()
        .map(|weight| floor + (f64::from(remaining) * weight / total).floor() as u32)
        .collect::<Vec<_>>();
    let assigned = target.iter().sum::<u32>();
    let mut order = (0..REGIONS).collect::<Vec<_>>();
    order.sort_unstable_by(|left, right| weights[*right].total_cmp(&weights[*left]));
    for region in order.into_iter().cycle().take((PIVOTS - assigned) as usize) {
        target[region] += 1;
    }

    if matches!(policy, Policy::Remodeling) {
        for (next, old) in target.iter_mut().zip(current) {
            if next.abs_diff(*old) <= 2 {
                *next = *old;
            }
        }
        rebalance(&mut target);
    }
    target
}

fn rebalance(values: &mut [u32]) {
    while values.iter().sum::<u32>() > PIVOTS {
        let (index, _) = values
            .iter()
            .enumerate()
            .filter(|(_, value)| **value > 2)
            .max_by_key(|(_, value)| **value)
            .expect("excess allocation has a removable pivot");
        values[index] -= 1;
    }
    while values.iter().sum::<u32>() < PIVOTS {
        let (index, _) = values
            .iter()
            .enumerate()
            .min_by_key(|(_, value)| **value)
            .expect("regions are non-empty");
        values[index] += 1;
    }
}
