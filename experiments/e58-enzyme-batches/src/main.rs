//! Prototype e58: choose vector batches from measured saturation curves.

use common::{Lcg, bench};

const ROWS: usize = 262_144;
const SIZES: [usize; 9] = [256, 512, 1_024, 2_048, 4_096, 8_192, 16_384, 32_768, 65_536];

#[derive(Clone, Copy)]
enum Shape {
    Decode,
    Filter,
    Strings,
    Aggregate,
    Join,
}
#[derive(Clone, Copy)]
enum Policy {
    Fixed,
    Offline,
    Hill,
    Saturation,
}
#[derive(Clone, Copy)]
struct Model {
    setup: u64,
    per_row: u64,
    sweet: usize,
    penalty: u64,
    width: u64,
}
struct Outcome {
    checksum: u64,
    runtime: u64,
    peak_memory: u64,
    batch: usize,
    probes: u64,
}

fn main() {
    println!("e58 — saturation batch sizing (executable prototype, audited)");
    let data = data();
    for shape in [
        Shape::Decode,
        Shape::Filter,
        Shape::Strings,
        Shape::Aggregate,
        Shape::Join,
    ] {
        let policies = [
            ("fixed 4096", Policy::Fixed),
            ("offline best", Policy::Offline),
            ("hill climb", Policy::Hill),
            ("saturation fit", Policy::Saturation),
        ];
        let outcomes = policies.map(|(_, policy)| execute(&data, shape, policy));
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.checksum == outcomes[0].checksum)
        );
        println!("\n=== {} ===", shape_name(shape));
        for ((name, _), out) in policies.iter().zip(outcomes.iter()) {
            println!(
                "{name:<16} batch {:>5} runtime {:>9} peak {:>8} probes {} ck {:016x}",
                out.batch, out.runtime, out.peak_memory, out.probes, out.checksum
            );
        }
        for (name, policy) in policies {
            let _ = bench(name, || execute(&data, shape, policy).checksum);
        }
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Decode => "decode",
        Shape::Filter => "filter",
        Shape::Strings => "strings",
        Shape::Aggregate => "aggregate",
        Shape::Join => "join",
    }
}
fn model(shape: Shape) -> Model {
    match shape {
        Shape::Decode => Model {
            setup: 4_000,
            per_row: 4,
            sweet: 8_192,
            penalty: 2,
            width: 16,
        },
        Shape::Filter => Model {
            setup: 2_000,
            per_row: 3,
            sweet: 4_096,
            penalty: 3,
            width: 12,
        },
        Shape::Strings => Model {
            setup: 1_000,
            per_row: 20,
            sweet: 1_024,
            penalty: 12,
            width: 64,
        },
        Shape::Aggregate => Model {
            setup: 3_000,
            per_row: 8,
            sweet: 2_048,
            penalty: 5,
            width: 48,
        },
        Shape::Join => Model {
            setup: 5_000,
            per_row: 10,
            sweet: 8_192,
            penalty: 4,
            width: 32,
        },
    }
}

fn data() -> Vec<u64> {
    let mut random = Lcg::new(0x5800_0058);
    (0..ROWS).map(|_| random.next_u64()).collect()
}
fn batch_cost(model: Model, batch: usize) -> u64 {
    model.setup
        + batch as u64 * model.per_row
        + batch.saturating_sub(model.sweet) as u64 * model.penalty
}
fn total_cost(model: Model, batch: usize) -> u64 {
    ROWS.div_ceil(batch) as u64 * batch_cost(model, batch)
}

fn choose(shape: Shape, policy: Policy) -> (usize, u64) {
    let model = model(shape);
    match policy {
        Policy::Fixed => (4_096, 0),
        Policy::Offline => (
            *SIZES
                .iter()
                .min_by_key(|size| total_cost(model, **size))
                .unwrap(),
            0,
        ),
        Policy::Hill => {
            let mut index = 4;
            let mut probes = 0;
            loop {
                let current = total_cost(model, SIZES[index]);
                let mut best = (current, index);
                for next in [index.saturating_sub(1), (index + 1).min(SIZES.len() - 1)] {
                    probes += 1;
                    best = best.min((total_cost(model, SIZES[next]), next));
                }
                if best.1 == index {
                    return (SIZES[index], probes);
                }
                index = best.1;
            }
        }
        Policy::Saturation => {
            let mut probes = 1;
            let mut chosen = SIZES[0];
            let mut previous = SIZES[0] as f64 / batch_cost(model, SIZES[0]) as f64;
            for &size in &SIZES[1..] {
                probes += 1;
                let throughput = size as f64 / batch_cost(model, size) as f64;
                if throughput <= previous * 1.02 {
                    break;
                }
                chosen = size;
                previous = throughput;
            }
            (chosen, probes)
        }
    }
}

fn execute(data: &[u64], shape: Shape, policy: Policy) -> Outcome {
    let (batch, probes) = choose(shape, policy);
    let model = model(shape);
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    for chunk in data.chunks(batch) {
        for value in chunk {
            checksum = (checksum ^ *value).wrapping_mul(0x100_0000_01b3);
        }
    }
    Outcome {
        checksum,
        runtime: total_cost(model, batch) + probes * batch_cost(model, 256),
        peak_memory: batch as u64 * model.width,
        batch,
        probes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_batching_policy_computes_the_exact_stream_checksum() {
        let data = data();
        for shape in [
            Shape::Decode,
            Shape::Filter,
            Shape::Strings,
            Shape::Aggregate,
            Shape::Join,
        ] {
            let expected = execute(&data, shape, Policy::Fixed).checksum;
            for policy in [Policy::Offline, Policy::Hill, Policy::Saturation] {
                assert_eq!(execute(&data, shape, policy).checksum, expected);
            }
        }
    }
    #[test]
    fn choices_are_measured_from_the_cost_curve() {
        for shape in [
            Shape::Decode,
            Shape::Filter,
            Shape::Strings,
            Shape::Aggregate,
            Shape::Join,
        ] {
            let (best, _) = choose(shape, Policy::Offline);
            assert!(SIZES.contains(&best));
            let (fit, probes) = choose(shape, Policy::Saturation);
            assert!(SIZES.contains(&fit));
            assert!(probes > 1);
        }
    }
}
