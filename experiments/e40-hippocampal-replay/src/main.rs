//! e40: replay expensive weakly predicted traces to choose bounded structures.
//!
//! Evidence tier: deterministic maintenance-policy simulation. Build and replay
//! costs are charged in the same work currency as future query savings.

use common::{Lcg, bench, check_consistency};

const WINDOWS: usize = 240;
const TEMPLATES: usize = 16;
const SLOTS: usize = 5;
const BUILDS_PER_WINDOW: usize = 2;

#[derive(Clone, Copy)]
enum Shape {
    Stable,
    Periodic,
    Drift,
}

#[derive(Clone)]
struct Window {
    counts: [u32; TEMPLATES],
    base_cost: [u32; TEMPLATES],
    prediction_error: [u32; TEMPLATES],
}

#[derive(Clone, Copy)]
enum Policy {
    Frequency,
    TotalCost,
    WorstError,
    WeakReplay,
}

struct Outcome {
    checksum: u64,
    work: u64,
    p95: u64,
    builds: u64,
    replay: u64,
}

fn main() {
    println!("e40 — hippocampal weak-trace replay (simulation tier)");
    println!("{WINDOWS} windows, {TEMPLATES} templates, {SLOTS} structures\n");
    for shape in [Shape::Stable, Shape::Periodic, Shape::Drift] {
        let windows = windows(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("frequency replay", Policy::Frequency),
            ("total-cost replay", Policy::TotalCost),
            ("worst-error replay", Policy::WorstError),
            ("weak-trace benefit replay", Policy::WeakReplay),
        ];
        let mut expected = None;
        for (name, policy) in policies {
            let outcome = run(&windows, policy);
            if let Some(checksum) = expected {
                assert_eq!(checksum, outcome.checksum);
            } else {
                expected = Some(outcome.checksum);
            }
            println!(
                "{name:<27} work {:>12}  p95 {:>5}  build {:>8}  replay {:>7}",
                outcome.work, outcome.p95, outcome.builds, outcome.replay
            );
        }
        let results = [
            bench("frequency", || run(&windows, Policy::Frequency).checksum),
            bench("cost", || run(&windows, Policy::TotalCost).checksum),
            bench("error", || run(&windows, Policy::WorstError).checksum),
            bench("weak replay", || run(&windows, Policy::WeakReplay).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Stable => "stable mixed workload",
        Shape::Periodic => "periodic rare expensive templates",
        Shape::Drift => "abrupt template-value drift",
    }
}

fn windows(shape: Shape) -> Vec<Window> {
    let mut random = Lcg::new(0xB1A1_0040_u64);
    (0..WINDOWS)
        .map(|window| {
            let phase = if matches!(shape, Shape::Drift) && window >= WINDOWS / 2 {
                1
            } else {
                0
            };
            let mut counts = [0_u32; TEMPLATES];
            let mut base_cost = [0_u32; TEMPLATES];
            let mut prediction_error = [0_u32; TEMPLATES];
            for template in 0..TEMPLATES {
                counts[template] = 2 + random.below(30) as u32;
                base_cost[template] = 40 + ((template * 37 + phase * 113) % 320) as u32;
                prediction_error[template] = 1 + ((template * 17 + phase * 7) % 10) as u32;
            }
            if matches!(shape, Shape::Periodic) {
                let rare = (window / 20) % TEMPLATES;
                counts[rare] = 4;
                base_cost[rare] = 700;
                prediction_error[rare] = 12;
            }
            Window {
                counts,
                base_cost,
                prediction_error,
            }
        })
        .collect()
}

fn run(windows: &[Window], policy: Policy) -> Outcome {
    let mut resident = [false; TEMPLATES];
    let mut previous = windows[0].clone();
    let mut work = 0_u64;
    let mut builds = 0_u64;
    let mut replay = 0_u64;
    let mut latencies = Vec::new();
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;

    for (window_index, window) in windows.iter().enumerate() {
        if window_index > 0 {
            let mut order = (0..TEMPLATES).collect::<Vec<_>>();
            order.sort_unstable_by_key(|template| {
                std::cmp::Reverse(score(&previous, *template, policy))
            });
            let wanted = &order[..SLOTS];
            let mut built = 0;
            for template in wanted {
                replay += u64::from(previous.base_cost[*template] / 4 + 1);
                if !resident[*template] && built < BUILDS_PER_WINDOW {
                    let build = build_cost(*template);
                    work += build;
                    builds += build;
                    resident[*template] = true;
                    built += 1;
                }
            }
            for (template, present) in resident.iter_mut().enumerate() {
                if !wanted.contains(&template) {
                    *present = false;
                }
            }
        }
        for (template, present) in resident.iter().enumerate() {
            let latency = if *present {
                window.base_cost[template] / 5 + 3
            } else {
                window.base_cost[template]
            };
            work += u64::from(latency) * u64::from(window.counts[template]);
            latencies.extend(std::iter::repeat_n(
                u64::from(latency),
                window.counts[template] as usize,
            ));
            checksum = (checksum ^ ((window_index * TEMPLATES + template) as u64))
                .wrapping_mul(0x100_0000_01b3);
        }
        previous = window.clone();
    }
    work += replay;
    latencies.sort_unstable();
    Outcome {
        checksum,
        work,
        p95: latencies[latencies.len() * 95 / 100],
        builds,
        replay,
    }
}

fn score(window: &Window, template: usize, policy: Policy) -> u64 {
    match policy {
        Policy::Frequency => u64::from(window.counts[template]),
        Policy::TotalCost => {
            u64::from(window.counts[template]) * u64::from(window.base_cost[template])
        }
        Policy::WorstError => u64::from(window.prediction_error[template]) * 1_000,
        Policy::WeakReplay => {
            u64::from(window.counts[template])
                * u64::from(window.base_cost[template])
                * u64::from(window.prediction_error[template])
                / build_cost(template).max(1)
        }
    }
}

fn build_cost(template: usize) -> u64 {
    2_000 + (template as u64 % 5) * 500
}
