//! e55: adjust prefetch aperture from useful-byte gain versus waste and pressure.
//!
//! Evidence tier: deterministic I/O policy simulation. Each run represents a
//! contiguous useful span terminated by pruning or query completion.

use common::{Lcg, bench, check_consistency};

const RUNS: usize = 12_000;
const DEPTHS: [u32; 4] = [1, 4, 16, 64];
const REQUEST_LATENCY: u64 = 40;

#[derive(Clone, Copy)]
enum Shape {
    Sequential,
    Pruned,
    Alternating,
    Pressure,
}

#[derive(Clone, Copy)]
struct Span {
    useful: u32,
    pressure: bool,
    token: u64,
}

struct Outcome {
    checksum: u64,
    cost: u64,
    wasted: u64,
    requests: u64,
    reversals: u64,
}

fn main() {
    println!("e55 — stomatal prefetch gates (simulation tier)");
    println!("{RUNS} useful spans; depths {DEPTHS:?}; request latency {REQUEST_LATENCY}\n");
    for shape in [
        Shape::Sequential,
        Shape::Pruned,
        Shape::Alternating,
        Shape::Pressure,
    ] {
        let trace = trace(shape);
        println!("=== {} ===", shape_name(shape));
        let fixed = DEPTHS.map(|depth| run_fixed(&trace, depth));
        for (depth, outcome) in DEPTHS.iter().zip(&fixed) {
            report(&format!("fixed depth {depth}"), outcome);
        }
        let adaptive = run_stomatal(&trace);
        report("stomatal hysteresis", &adaptive);
        for outcome in &fixed {
            assert_eq!(outcome.checksum, adaptive.checksum);
        }
        let best = fixed
            .iter()
            .map(|value| value.cost)
            .min()
            .expect("fixed policies");
        println!(
            "adaptive vs offline fixed best: {:+.1}%\n",
            adaptive.cost as f64 * 100.0 / best as f64 - 100.0
        );

        let results = [
            bench("fixed depth 1", || run_fixed(&trace, 1).checksum),
            bench("fixed depth 4", || run_fixed(&trace, 4).checksum),
            bench("fixed depth 16", || run_fixed(&trace, 16).checksum),
            bench("fixed depth 64", || run_fixed(&trace, 64).checksum),
            bench("stomatal", || run_stomatal(&trace).checksum),
        ];
        check_consistency(&results);
    }
}

fn report(name: &str, outcome: &Outcome) {
    println!(
        "{name:<24} cost {:>11}  requests {:>8}  wasted {:>9}  reversals {:>4}",
        outcome.cost, outcome.requests, outcome.wasted, outcome.reversals
    );
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Sequential => "long sequential scans",
        Shape::Pruned => "aggressively pruned spans",
        Shape::Alternating => "four abrupt long/short phases",
        Shape::Pressure => "mixed spans with memory pressure",
    }
}

fn trace(shape: Shape) -> Vec<Span> {
    let mut random = Lcg::new(0x570A_0055_u64);
    (0..RUNS)
        .map(|index| {
            let phase = index / (RUNS / 4);
            let useful = match shape {
                Shape::Sequential => 192 + random.below(129) as u32,
                Shape::Pruned => 1 + random.below(5) as u32,
                Shape::Alternating if phase.is_multiple_of(2) => 192 + random.below(129) as u32,
                Shape::Alternating => 1 + random.below(5) as u32,
                Shape::Pressure => {
                    if random.below(100) < 55 {
                        48 + random.below(145) as u32
                    } else {
                        1 + random.below(12) as u32
                    }
                }
            };
            Span {
                useful,
                pressure: matches!(shape, Shape::Pressure) && index % 17 < 7,
                token: random.next_u64(),
            }
        })
        .collect()
}

fn run_fixed(trace: &[Span], depth: u32) -> Outcome {
    let mut outcome = Outcome {
        checksum: 0xcbf2_9ce4_8422_2325,
        cost: 0,
        wasted: 0,
        requests: 0,
        reversals: 0,
    };
    for span in trace {
        apply_span(&mut outcome, *span, depth);
    }
    outcome
}

fn run_stomatal(trace: &[Span]) -> Outcome {
    let mut outcome = Outcome {
        checksum: 0xcbf2_9ce4_8422_2325,
        cost: 0,
        wasted: 0,
        requests: 0,
        reversals: 0,
    };
    let mut depth = 16_u32;
    let mut desired = depth;
    let mut agreement = 0_u8;
    let mut previous_direction = 0_i8;

    for span in trace {
        apply_span(&mut outcome, *span, depth);
        let chunks = span.useful.div_ceil(depth);
        let fetched = chunks * depth;
        let waste_ratio = (fetched - span.useful) as f64 / fetched as f64;
        let next = if span.pressure || waste_ratio > 0.30 {
            (depth / 2).max(1)
        } else if span.useful >= depth * 3 && waste_ratio < 0.12 {
            (depth * 2).min(64)
        } else {
            depth
        };
        if next == desired && next != depth {
            agreement += 1;
        } else {
            desired = next;
            agreement = u8::from(next != depth);
        }
        if agreement >= 3 {
            let direction = if desired > depth { 1 } else { -1 };
            if previous_direction != 0 && direction != previous_direction {
                outcome.reversals += 1;
            }
            previous_direction = direction;
            depth = desired;
            agreement = 0;
        }
    }
    outcome
}

fn apply_span(outcome: &mut Outcome, span: Span, depth: u32) {
    let requests = span.useful.div_ceil(depth);
    let fetched = requests * depth;
    let wasted = fetched - span.useful;
    let pressure_cost = if span.pressure {
        u64::from(depth) * 2
    } else {
        0
    };
    outcome.requests += u64::from(requests);
    outcome.wasted += u64::from(wasted);
    outcome.cost += u64::from(requests) * REQUEST_LATENCY + u64::from(fetched) + pressure_cost;
    outcome.checksum =
        (outcome.checksum ^ span.token ^ u64::from(span.useful)).wrapping_mul(0x100_0000_01b3);
}
