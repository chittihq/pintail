//! e46: compact only when neighboring segment signals form a local quorum.
//!
//! Evidence tier: deterministic lifecycle simulation. Logical interval versions
//! produce identical query answers regardless of physical compaction policy.

use common::{Lcg, bench, check_consistency};

const INTERVALS: usize = 128;
const OPERATIONS: usize = 80_000;
const BASE_BYTES: u64 = 64 * 1024;
const MAINTENANCE_EVERY: usize = 100;
const COMPACTIONS_PER_WINDOW: usize = 2;

#[derive(Clone, Copy)]
enum Shape {
    RecentHot,
    MovingHot,
    Scattered,
    AppendOnly,
}

#[derive(Clone, Copy)]
enum Op {
    Update {
        interval: usize,
        tombstone: bool,
        overlap: bool,
    },
    Query {
        interval: usize,
    },
}

#[derive(Clone, Copy)]
enum Policy {
    FixedCount,
    GlobalScore,
    Quorum,
}

#[derive(Clone)]
struct Cell {
    version: u64,
    overlaps: u32,
    tombstones: u32,
    heat: f64,
}

struct Outcome {
    checksum: u64,
    read_bytes: u64,
    write_bytes: u64,
    p99_read: u64,
    compactions: u64,
}

fn main() {
    println!("e46 — quorum-sensing compaction (simulation tier)");
    println!(
        "{INTERVALS} key intervals, {OPERATIONS} operations, {BASE_BYTES} bytes/base interval\n"
    );
    for shape in [
        Shape::RecentHot,
        Shape::MovingHot,
        Shape::Scattered,
        Shape::AppendOnly,
    ] {
        let trace = trace(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("fixed overlap count", Policy::FixedCount),
            ("global pain score", Policy::GlobalScore),
            ("local quorum", Policy::Quorum),
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
                "{name:<23} read {:>12}  write {:>11}  total {:>12}  p99 {:>9}  compactions {:>4}",
                outcome.read_bytes,
                outcome.write_bytes,
                outcome.read_bytes + outcome.write_bytes,
                outcome.p99_read,
                outcome.compactions
            );
        }
        let results = [
            bench("fixed", || run(&trace, Policy::FixedCount).checksum),
            bench("global", || run(&trace, Policy::GlobalScore).checksum),
            bench("quorum", || run(&trace, Policy::Quorum).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::RecentHot => "recent-hot updates and reads",
        Shape::MovingHot => "moving hot neighborhood",
        Shape::Scattered => "uniform scattered updates",
        Shape::AppendOnly => "append-only hostile control",
    }
}

fn trace(shape: Shape) -> Vec<Op> {
    let mut random = Lcg::new(0xBAC7_0046_u64);
    (0..OPERATIONS)
        .map(|index| {
            let phase = index / (OPERATIONS / 4);
            let hot = match shape {
                Shape::MovingHot => (13 + phase * 29) % (INTERVALS - 12),
                _ => 47,
            };
            let hot_interval = hot + random.below(12) as usize;
            let interval = match shape {
                Shape::RecentHot | Shape::MovingHot if random.below(100) < 85 => hot_interval,
                _ => random.below(INTERVALS as u64) as usize,
            };
            if matches!(shape, Shape::AppendOnly) {
                if random.below(100) < 45 {
                    Op::Update {
                        interval,
                        tombstone: false,
                        overlap: false,
                    }
                } else {
                    Op::Query { interval }
                }
            } else if random.below(100) < 42 {
                Op::Update {
                    interval,
                    tombstone: random.below(10) == 0,
                    overlap: true,
                }
            } else {
                Op::Query { interval }
            }
        })
        .collect()
}

fn run(trace: &[Op], policy: Policy) -> Outcome {
    let mut cells = vec![
        Cell {
            version: 0,
            overlaps: 0,
            tombstones: 0,
            heat: 0.0
        };
        INTERVALS
    ];
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut read_bytes = 0_u64;
    let mut write_bytes = 0_u64;
    let mut read_costs = Vec::new();
    let mut compactions = 0_u64;

    for (index, op) in trace.iter().enumerate() {
        match *op {
            Op::Update {
                interval,
                tombstone,
                overlap,
            } => {
                let cell = &mut cells[interval];
                cell.version += 1;
                if overlap {
                    cell.overlaps += 1;
                    cell.tombstones += u32::from(tombstone);
                }
            }
            Op::Query { interval } => {
                let cell = &mut cells[interval];
                let bytes = BASE_BYTES * (1 + u64::from(cell.overlaps));
                read_bytes += bytes;
                read_costs.push(bytes);
                cell.heat += 1.0;
                checksum =
                    (checksum ^ cell.version ^ interval as u64).wrapping_mul(0x100_0000_01b3);
            }
        }
        if (index + 1) % MAINTENANCE_EVERY == 0 {
            for _ in 0..COMPACTIONS_PER_WINDOW {
                let Some(interval) = choose(&cells, policy) else {
                    break;
                };
                let cell = &mut cells[interval];
                write_bytes += BASE_BYTES * (1 + u64::from(cell.overlaps));
                cell.overlaps = 0;
                cell.tombstones = 0;
                compactions += 1;
            }
            cells.iter_mut().for_each(|cell| cell.heat *= 0.88);
        }
    }
    read_costs.sort_unstable();
    Outcome {
        checksum,
        read_bytes,
        write_bytes,
        p99_read: read_costs[read_costs.len() * 99 / 100],
        compactions,
    }
}

fn choose(cells: &[Cell], policy: Policy) -> Option<usize> {
    match policy {
        Policy::FixedCount => cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.overlaps >= 6)
            .max_by_key(|(_, cell)| cell.overlaps)
            .map(|(index, _)| index),
        Policy::GlobalScore => cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.overlaps > 0)
            .max_by(|(_, left), (_, right)| pain(left).total_cmp(&pain(right)))
            .map(|(index, _)| index),
        Policy::Quorum => cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.overlaps > 0)
            .filter_map(|(index, cell)| {
                let start = index.saturating_sub(1);
                let end = (index + 2).min(cells.len());
                let neighborhood = cells[start..end].iter().map(signal).sum::<f64>();
                (neighborhood >= 16.0).then_some((index, neighborhood * (1.0 + cell.heat)))
            })
            .max_by(|(_, left), (_, right)| left.total_cmp(right))
            .map(|(index, _)| index),
    }
}

fn pain(cell: &Cell) -> f64 {
    f64::from(cell.overlaps) * (1.0 + cell.heat) + f64::from(cell.tombstones) * 2.0
}

fn signal(cell: &Cell) -> f64 {
    f64::from(cell.overlaps) * 2.0 + f64::from(cell.tombstones) * 3.0 + cell.heat.sqrt()
}
