//! e50: couple maintenance aperture to query, CDC, debt, and capacity signals.
//!
//! Evidence tier: deterministic control simulation. Work is conserved and all
//! policies drain the same demand; only backlog/debt trajectories differ.

use common::{Lcg, bench, check_consistency};

const TICKS: usize = 20_000;

#[derive(Clone, Copy)]
enum Shape {
    Periodic,
    Spike,
    Noisy,
}

#[derive(Clone, Copy)]
enum Policy {
    Fixed,
    Independent,
    Ventilation,
}

struct Trace {
    query: Vec<u64>,
    cdc: Vec<u64>,
    debt: Vec<u64>,
    capacity: Vec<u64>,
}

struct Outcome {
    checksum: u64,
    debt_area: u128,
    queue_area: u128,
    max_cdc: u64,
    slo_ticks: u64,
    reversals: u64,
    makespan: u64,
}

fn main() {
    println!("e50 — termite-mound maintenance ventilation (simulation tier)");
    println!("{TICKS} arrival ticks; coupled query/CDC/debt/capacity feedback\n");
    for shape in [Shape::Periodic, Shape::Spike, Shape::Noisy] {
        let trace = trace(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("fixed 15% aperture", Policy::Fixed),
            ("independent debt PID", Policy::Independent),
            ("coupled ventilation", Policy::Ventilation),
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
                "{name:<24} debt-area {:>13}  queue-area {:>13}  max CDC {:>6}  SLO {:>5}  reversals {:>4}  end {:>6}",
                outcome.debt_area,
                outcome.queue_area,
                outcome.max_cdc,
                outcome.slo_ticks,
                outcome.reversals,
                outcome.makespan
            );
        }
        let results = [
            bench("fixed", || run(&trace, Policy::Fixed).checksum),
            bench("independent", || run(&trace, Policy::Independent).checksum),
            bench("ventilation", || run(&trace, Policy::Ventilation).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Periodic => "periodic load",
        Shape::Spike => "unexpected query/CDC spike",
        Shape::Noisy => "10% noisy shared capacity",
    }
}

fn trace(shape: Shape) -> Trace {
    let mut random = Lcg::new(0x7E87_0050_u64);
    let mut query = Vec::with_capacity(TICKS);
    let mut cdc = Vec::with_capacity(TICKS);
    let mut debt = Vec::with_capacity(TICKS);
    let mut capacity = Vec::with_capacity(TICKS);
    for tick in 0..TICKS {
        let period = tick % 1_000;
        let busy = period < 600;
        let spike = matches!(shape, Shape::Spike) && (8_000..11_000).contains(&tick);
        query.push(if spike {
            86
        } else if busy {
            62
        } else {
            25
        });
        cdc.push(if spike { 25 } else { 13 });
        debt.push(if busy { 8 } else { 4 });
        let noise = if matches!(shape, Shape::Noisy) {
            random.below(21) as i64 - 10
        } else {
            0
        };
        capacity.push((100_i64 + noise).max(70) as u64);
    }
    Trace {
        query,
        cdc,
        debt,
        capacity,
    }
}

fn run(trace: &Trace, policy: Policy) -> Outcome {
    let total_input = trace.query.iter().sum::<u64>()
        + trace.cdc.iter().sum::<u64>()
        + trace.debt.iter().sum::<u64>();
    let mut query_queue = 0_u64;
    let mut cdc_lag = 0_u64;
    let mut debt = 4_000_u64;
    let mut debt_area = 0_u128;
    let mut queue_area = 0_u128;
    let mut max_cdc = 0_u64;
    let mut slo_ticks = 0_u64;
    let mut tick = 0_usize;
    let mut aperture = 15_u64;
    let mut previous_direction = 0_i8;
    let mut reversals = 0_u64;

    while tick < TICKS || query_queue > 0 || cdc_lag > 0 || debt > 0 {
        let capacity = if tick < TICKS {
            trace.capacity[tick]
        } else {
            100
        };
        if tick < TICKS {
            query_queue += trace.query[tick];
            cdc_lag += trace.cdc[tick];
            debt += trace.debt[tick];
        }
        let next_aperture = match policy {
            Policy::Fixed => 15,
            Policy::Independent => (5 + debt / 250).clamp(5, 45),
            Policy::Ventilation => {
                let debt_push = (debt / 300).min(35) as i64;
                let query_pull = (query_queue / 150).min(20) as i64;
                let cdc_pull = (cdc_lag / 60).min(20) as i64;
                let target = (8 + debt_push - query_pull - cdc_pull).clamp(3, 42) as u64;
                if target > aperture + 3 {
                    aperture + 3
                } else if target + 5 < aperture {
                    aperture.saturating_sub(5).max(3)
                } else {
                    aperture
                }
            }
        };
        if matches!(policy, Policy::Ventilation) && next_aperture != aperture {
            let direction = if next_aperture > aperture { 1 } else { -1 };
            if previous_direction != 0 && direction != previous_direction {
                reversals += 1;
            }
            previous_direction = direction;
        }
        aperture = next_aperture;

        let maintenance_budget = capacity * aperture / 100;
        let cdc_budget = (capacity / 5).max(12).min(capacity - maintenance_budget);
        let cdc_done = cdc_lag.min(cdc_budget);
        cdc_lag -= cdc_done;
        let query_budget = capacity - maintenance_budget - cdc_done;
        let query_done = query_queue.min(query_budget);
        query_queue -= query_done;
        let unused = query_budget - query_done;
        let maintenance_done = debt.min(maintenance_budget + unused);
        debt -= maintenance_done;

        debt_area += u128::from(debt);
        queue_area += u128::from(query_queue);
        max_cdc = max_cdc.max(cdc_lag);
        if query_queue > 500 || cdc_lag > 200 {
            slo_ticks += 1;
        }
        tick += 1;
        assert!(tick < 100_000, "controller must drain");
    }
    Outcome {
        checksum: total_input ^ 0x5050_5050,
        debt_area,
        queue_area,
        max_cdc,
        slo_ticks,
        reversals,
        makespan: tick as u64,
    }
}
