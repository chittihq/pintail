//! e34: grow and retire range-local equality indexes under a global byte budget.
//!
//! Evidence tier: deterministic policy simulation. Counts come from an exact
//! histogram shared by every policy; the model charges rows scanned and built.

use common::{Lcg, bench, check_consistency};

const ROWS: u64 = 4_000_000;
const DOMAIN: usize = 65_536;
const PATCHES: usize = 256;
const BUDGET_PATCHES: usize = 20;
const PHASE_QUERIES: usize = 8_000;
const EPOCH: usize = 200;

#[derive(Clone, Copy)]
enum Shape {
    Stationary,
    Migrating,
    Decoy,
}

#[derive(Clone, Copy)]
enum Policy {
    Scan,
    Global,
    Frequency,
    Root,
}

struct Outcome {
    checksum: u64,
    work: u64,
    builds: u64,
    retirements: u64,
}

fn main() {
    println!("e34 — root-foraging micro-indexes (simulation tier)");
    println!("{ROWS} rows, {PATCHES} value patches, budget {BUDGET_PATCHES} patches\n");
    let histogram = histogram();

    for shape in [Shape::Stationary, Shape::Migrating, Shape::Decoy] {
        let trace = trace(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("no index", Policy::Scan),
            ("one global secondary index", Policy::Global),
            ("frequency-only rootlets", Policy::Frequency),
            ("local benefit + systemic cap", Policy::Root),
        ];
        let mut expected = None;
        for (name, policy) in policies {
            let outcome = run(&trace, &histogram, policy);
            if let Some(checksum) = expected {
                assert_eq!(checksum, outcome.checksum);
            } else {
                expected = Some(outcome.checksum);
            }
            println!(
                "{name:<31} work {:>14} rows  builds {:>8}  retired {:>5}",
                outcome.work, outcome.builds, outcome.retirements
            );
        }
        let results = [
            bench("scan", || run(&trace, &histogram, Policy::Scan).checksum),
            bench("global", || {
                run(&trace, &histogram, Policy::Global).checksum
            }),
            bench("frequency", || {
                run(&trace, &histogram, Policy::Frequency).checksum
            }),
            bench("root foraging", || {
                run(&trace, &histogram, Policy::Root).checksum
            }),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Stationary => "stationary nutrient patch",
        Shape::Migrating => "migrating patch",
        Shape::Decoy => "short decoy burst then stable patch",
    }
}

fn histogram() -> Vec<u32> {
    let mut values = vec![0_u32; DOMAIN];
    let mut random = Lcg::new(0xA007_0034_u64);
    for _ in 0..ROWS {
        values[random.below(DOMAIN as u64) as usize] += 1;
    }
    values
}

fn trace(shape: Shape) -> Vec<u16> {
    let phases = if matches!(shape, Shape::Stationary) {
        1
    } else {
        4
    };
    let mut random = Lcg::new(0xF0A6_0034_u64);
    let mut result = Vec::with_capacity(phases * PHASE_QUERIES);
    for phase in 0..phases {
        let hot_patch = match shape {
            Shape::Stationary => 37,
            Shape::Migrating => (37 + phase * 53) % PATCHES,
            Shape::Decoy => {
                if phase == 0 {
                    211
                } else {
                    37
                }
            }
        };
        let hot_probability = if matches!(shape, Shape::Decoy) && phase == 0 {
            95
        } else {
            82
        };
        for _ in 0..PHASE_QUERIES {
            let patch = if random.below(100) < hot_probability {
                hot_patch
            } else {
                random.below(PATCHES as u64) as usize
            };
            let value =
                patch * (DOMAIN / PATCHES) + random.below((DOMAIN / PATCHES) as u64) as usize;
            result.push(value as u16);
        }
    }
    result
}

fn run(trace: &[u16], histogram: &[u32], policy: Policy) -> Outcome {
    let rows_per_patch = ROWS / PATCHES as u64;
    let mut active = vec![false; PATCHES];
    let mut scores = vec![0.0_f64; PATCHES];
    let mut work = if matches!(policy, Policy::Global) {
        ROWS
    } else {
        0
    };
    let mut builds = work;
    let mut retirements = 0_u64;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;

    for (index, value) in trace.iter().enumerate() {
        let patch = usize::from(*value) / (DOMAIN / PATCHES);
        let indexed = matches!(policy, Policy::Global) || active[patch];
        let exact_rows = u64::from(histogram[usize::from(*value)]).max(1);
        let query_work = if indexed { exact_rows } else { ROWS };
        work += query_work;
        let count = u64::from(histogram[usize::from(*value)]);
        checksum = (checksum ^ (count + u64::from(*value))).wrapping_mul(0x100_0000_01b3);
        scores[patch] += if matches!(policy, Policy::Root) {
            // Reward counterfactual savings so an unindexed patch can earn a
            // rootlet. Realized-savings feedback creates an incumbent trap.
            (ROWS - exact_rows) as f64 / rows_per_patch as f64
        } else {
            1.0
        };

        if (index + 1) % EPOCH == 0 && matches!(policy, Policy::Frequency | Policy::Root) {
            let mut order = (0..PATCHES).collect::<Vec<_>>();
            order.sort_unstable_by(|left, right| scores[*right].total_cmp(&scores[*left]));
            let mut wanted = vec![false; PATCHES];
            for patch in order.into_iter().take(BUDGET_PATCHES) {
                wanted[patch] = true;
            }
            for patch in 0..PATCHES {
                if wanted[patch] && !active[patch] {
                    active[patch] = true;
                    work += rows_per_patch;
                    builds += rows_per_patch;
                } else if !wanted[patch] && active[patch] {
                    active[patch] = false;
                    retirements += 1;
                }
            }
            if matches!(policy, Policy::Root) {
                scores.iter_mut().for_each(|score| *score *= 0.65);
            }
        }
    }
    Outcome {
        checksum,
        work,
        builds,
        retirements,
    }
}
