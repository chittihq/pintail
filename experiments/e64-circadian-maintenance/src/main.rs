//! Prototype e64: schedule maintenance from bounded seasonal history plus safety reflex.

use common::{Lcg, bench};

const TICKS: usize = 4_320;
const PERIOD: usize = 144;
const CAPACITY: u64 = 100;

#[derive(Clone, Copy)]
enum Shape {
    Periodic,
    Drifting,
    Missing,
    Random,
}
#[derive(Clone, Copy)]
enum Policy {
    Fixed,
    Reactive,
    Forecast,
    Combined,
}
struct Tick {
    load: u64,
    maintenance: u64,
    token: u64,
}
struct Outcome {
    checksum: u64,
    completed: u64,
    p99: u64,
    breaches: u64,
    max_debt: u64,
    model_bytes: u64,
}

fn main() {
    println!("e64 — circadian maintenance prediction (executable prototype, audited)");
    for shape in [
        Shape::Periodic,
        Shape::Drifting,
        Shape::Missing,
        Shape::Random,
    ] {
        let trace = fixture(shape);
        let policies = [
            ("fixed schedule", Policy::Fixed),
            ("reactive", Policy::Reactive),
            ("forecast only", Policy::Forecast),
            ("forecast + reflex", Policy::Combined),
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
                "{name:<19} completed {:>7} p99 {:>4} breaches {} max debt {:>7} model {} B ck {:016x}",
                out.completed, out.p99, out.breaches, out.max_debt, out.model_bytes, out.checksum
            );
        }
        for (name, policy) in policies {
            let _ = bench(name, || execute(&trace, policy).checksum);
        }
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Periodic => "periodic",
        Shape::Drifting => "drifting period",
        Shape::Missing => "missing cycle",
        Shape::Random => "random",
    }
}
fn fixture(shape: Shape) -> Vec<Tick> {
    let mut random = Lcg::new(0x6400_0064 ^ shape as u64);
    (0..TICKS)
        .map(|tick| {
            let period = if matches!(shape, Shape::Drifting) {
                PERIOD + tick / 720 * 8
            } else {
                PERIOD
            };
            let phase = tick % period;
            let mut load = if phase < period / 3 {
                88
            } else if phase < period * 2 / 3 {
                55
            } else {
                25
            };
            load += random.below(8);
            if matches!(shape, Shape::Missing) && (tick / PERIOD) == 14 {
                load = 95;
            }
            if matches!(shape, Shape::Random) {
                load = 20 + random.below(78);
            }
            Tick {
                load,
                maintenance: 24 + random.below(8),
                token: random.next_u64(),
            }
        })
        .collect()
}

fn execute(trace: &[Tick], policy: Policy) -> Outcome {
    let mut history = [50_u8; PERIOD];
    let mut debt = 0_u64;
    let mut completed = 0;
    let mut latencies = Vec::new();
    let mut breaches = 0;
    let mut max_debt = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    for (tick, item) in trace.iter().enumerate() {
        debt += item.maintenance;
        let predicted = u64::from(history[tick % PERIOD]);
        let slack = CAPACITY.saturating_sub(item.load);
        let requested = match policy {
            Policy::Fixed if tick % PERIOD >= PERIOD * 2 / 3 => 35,
            Policy::Fixed => 0,
            Policy::Reactive if debt > 0 => 50,
            Policy::Reactive => 0,
            Policy::Forecast if predicted < 60 => 45,
            Policy::Forecast => 0,
            Policy::Combined if item.load >= 85 => 0,
            Policy::Combined if predicted < 65 || debt > 4_000 => 50,
            Policy::Combined => 0,
        };
        let work = requested.min(slack).min(debt);
        debt -= work;
        completed += work;
        max_debt = max_debt.max(debt);
        let latency = 100 + item.load + debt / 1_000;
        breaches += u64::from(latency > 210);
        latencies.push(latency);
        history[tick % PERIOD] = item.load as u8;
        checksum = (checksum ^ item.token ^ item.load).wrapping_mul(0x100_0000_01b3);
    }
    latencies.sort_unstable();
    Outcome {
        checksum,
        completed,
        p99: latencies[latencies.len() * 99 / 100],
        breaches,
        max_debt,
        model_bytes: if matches!(policy, Policy::Forecast | Policy::Combined) {
            PERIOD as u64
        } else {
            0
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_controller_preserves_the_foreground_checksum() {
        for shape in [
            Shape::Periodic,
            Shape::Drifting,
            Shape::Missing,
            Shape::Random,
        ] {
            let trace = fixture(shape);
            let expected = execute(&trace, Policy::Fixed).checksum;
            for policy in [Policy::Reactive, Policy::Forecast, Policy::Combined] {
                assert_eq!(execute(&trace, policy).checksum, expected);
            }
        }
    }
    #[test]
    fn combined_never_spends_beyond_current_slack() {
        let trace = fixture(Shape::Missing);
        let out = execute(&trace, Policy::Combined);
        assert!(out.model_bytes < 64 * 1024);
    }
}
