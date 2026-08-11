//! e38: hysteretic overload mode protects CDC while preserving normal throughput.
//!
//! Evidence tier: deterministic queueing simulation with identical query/CDC work.

use std::collections::VecDeque;

use common::{Lcg, bench, check_consistency};

const ARRIVAL_TICKS: u64 = 12_000;
const CAPACITY: u64 = 100;
const CDC_BUFFER: u64 = 1_200;

#[derive(Clone, Copy)]
enum Shape {
    Mild,
    Sustained,
    Spike,
}

#[derive(Clone, Copy)]
enum Policy {
    Unconstrained,
    Conservative,
    Fever,
}

#[derive(Clone)]
struct Query {
    id: u64,
    arrival: u64,
    remaining: u64,
}

struct Trace {
    arrivals: Vec<Vec<Query>>,
    cdc: Vec<u64>,
}

struct Outcome {
    checksum: u64,
    makespan: u64,
    p99: u64,
    max_cdc_lag: u64,
    overflow_ticks: u64,
    mode_changes: u64,
}

fn main() {
    println!("e38 — fever-mode overload control (simulation tier)");
    println!("capacity {CAPACITY}/tick, CDC buffer {CDC_BUFFER}, {ARRIVAL_TICKS} arrival ticks\n");
    for shape in [Shape::Mild, Shape::Sustained, Shape::Spike] {
        let trace = make_trace(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("unconstrained", Policy::Unconstrained),
            ("fixed conservative", Policy::Conservative),
            ("fever hysteresis", Policy::Fever),
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
                "{name:<22} makespan {:>6}  p99 {:>6}  max CDC {:>6}  overflow {:>5}  changes {:>4}",
                outcome.makespan,
                outcome.p99,
                outcome.max_cdc_lag,
                outcome.overflow_ticks,
                outcome.mode_changes
            );
        }
        let results = [
            bench("unconstrained", || {
                run(&trace, Policy::Unconstrained).checksum
            }),
            bench("conservative", || {
                run(&trace, Policy::Conservative).checksum
            }),
            bench("fever", || run(&trace, Policy::Fever).checksum),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Mild => "mild steady load",
        Shape::Sustained => "sustained overload",
        Shape::Spike => "short extreme spike",
    }
}

fn make_trace(shape: Shape) -> Trace {
    let mut random = Lcg::new(0xFE8E_0038_u64);
    let mut arrivals = vec![Vec::new(); ARRIVAL_TICKS as usize];
    let mut cdc = vec![0_u64; ARRIVAL_TICKS as usize];
    let mut id = 0_u64;
    for tick in 0..ARRIVAL_TICKS {
        let intensity = match shape {
            Shape::Mild => 42,
            Shape::Sustained => 78,
            Shape::Spike if (4_000..7_000).contains(&tick) => 125,
            Shape::Spike => 35,
        };
        let mut work = intensity;
        while work > 0 {
            let query_work = (20 + random.below(81)).min(work);
            arrivals[tick as usize].push(Query {
                id,
                arrival: tick,
                remaining: query_work,
            });
            id += 1;
            work -= query_work;
        }
        cdc[tick as usize] = match shape {
            Shape::Mild => 12,
            Shape::Sustained => 18,
            Shape::Spike if (4_000..7_000).contains(&tick) => 24,
            Shape::Spike => 12,
        };
    }
    Trace { arrivals, cdc }
}

fn run(trace: &Trace, policy: Policy) -> Outcome {
    let mut queue = VecDeque::<Query>::new();
    let mut latencies = Vec::new();
    let mut tick = 0_u64;
    let mut cdc_lag = 0_u64;
    let mut max_cdc_lag = 0_u64;
    let mut overflow_ticks = 0_u64;
    let mut fever = false;
    let mut mode_changes = 0_u64;
    let mut completed = Vec::new();

    while tick < ARRIVAL_TICKS || !queue.is_empty() || cdc_lag > 0 {
        if tick < ARRIVAL_TICKS {
            queue.extend(trace.arrivals[tick as usize].iter().cloned());
            cdc_lag += trace.cdc[tick as usize];
        }

        if matches!(policy, Policy::Fever) {
            let enter = cdc_lag > 240 || queue.len() > 80;
            let exit = cdc_lag < 80 && queue.len() < 25;
            if !fever && enter {
                fever = true;
                mode_changes += 1;
            } else if fever && exit {
                fever = false;
                mode_changes += 1;
            }
        }
        let (slots, cdc_reserve) = match policy {
            Policy::Unconstrained => (10_usize, 5_u64),
            Policy::Conservative => (4, 22),
            Policy::Fever if fever => (4, 28),
            Policy::Fever => (8, 12),
        };

        let cdc_done = cdc_lag.min(cdc_reserve);
        cdc_lag -= cdc_done;
        let contention = if slots > 6 { (slots as u64 - 6) * 3 } else { 0 };
        let query_capacity = (CAPACITY - cdc_done).saturating_sub(contention);
        let query_done = service_queries(
            &mut queue,
            slots,
            query_capacity,
            tick,
            &mut latencies,
            &mut completed,
        );
        let extra_cdc = cdc_lag.min(query_capacity.saturating_sub(query_done));
        cdc_lag -= extra_cdc;
        max_cdc_lag = max_cdc_lag.max(cdc_lag);
        if cdc_lag > CDC_BUFFER {
            overflow_ticks += 1;
        }
        tick += 1;
        assert!(tick < 200_000, "simulation must drain");
    }
    latencies.sort_unstable();
    Outcome {
        checksum: checksum_ids(&mut completed) ^ trace.cdc.iter().sum::<u64>(),
        makespan: tick,
        p99: latencies[latencies.len() * 99 / 100],
        max_cdc_lag,
        overflow_ticks,
        mode_changes,
    }
}

fn service_queries(
    queue: &mut VecDeque<Query>,
    slots: usize,
    capacity: u64,
    tick: u64,
    latencies: &mut Vec<u64>,
    completed: &mut Vec<u64>,
) -> u64 {
    let active = slots.min(queue.len());
    if active == 0 {
        return 0;
    }
    let share = (capacity / active as u64).max(1);
    let mut consumed = 0_u64;
    for _ in 0..active {
        let mut query = queue.pop_front().expect("active query exists");
        let done = query.remaining.min(share);
        consumed += done;
        query.remaining -= done;
        if query.remaining == 0 {
            latencies.push(tick + 1 - query.arrival);
            completed.push(query.id);
        } else {
            queue.push_back(query);
        }
    }
    consumed
}

fn checksum_ids(ids: &mut [u64]) -> u64 {
    ids.sort_unstable();
    ids.iter().fold(0xcbf2_9ce4_8422_2325_u64, |checksum, id| {
        (checksum ^ id).wrapping_mul(0x100_0000_01b3)
    })
}
