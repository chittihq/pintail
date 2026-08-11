//! e63: keep a completion-only memory reserve to stop synchronized spill cascades.
//!
//! Evidence tier: deterministic batch-scheduler simulation. All tasks complete
//! and contribute the same checksum; policies differ in admission and finalization.

use std::collections::VecDeque;

use common::{Lcg, bench, check_consistency};

const TASKS: usize = 4_000;
const CAP: u32 = 1_000;
const RESERVE: u32 = 140;

#[derive(Clone, Copy)]
enum Shape {
    Steady,
    Synchronized,
    CdcCommit,
    Mixed,
}

#[derive(Clone, Copy)]
struct Task {
    id: u64,
    base: u32,
    burst: u32,
    work: u32,
    protected: bool,
}

#[derive(Clone, Copy)]
enum Policy {
    FullUse,
    PerTaskPadding,
    GlobalReserve,
    CompletionReserve,
}

struct Outcome {
    checksum: u64,
    makespan: u64,
    spill: u64,
    cascades: u64,
    max_batch: usize,
}

fn main() {
    println!("e63 — glycogen emergency reserve (simulation tier)");
    println!("{TASKS} tasks, cap {CAP}, reserve {RESERVE}\n");
    for shape in [
        Shape::Steady,
        Shape::Synchronized,
        Shape::CdcCommit,
        Shape::Mixed,
    ] {
        let tasks = tasks(shape);
        println!("=== {} ===", shape_name(shape));
        let policies = [
            ("full utilization", Policy::FullUse),
            ("per-task padding", Policy::PerTaskPadding),
            ("global reserve", Policy::GlobalReserve),
            ("completion-only reserve", Policy::CompletionReserve),
        ];
        let mut expected = None;
        for (name, policy) in policies {
            let outcome = run(&tasks, policy);
            if let Some(checksum) = expected {
                assert_eq!(checksum, outcome.checksum);
            } else {
                expected = Some(outcome.checksum);
            }
            println!(
                "{name:<25} makespan {:>8}  spill {:>10}  cascades {:>5}  max batch {:>3}",
                outcome.makespan, outcome.spill, outcome.cascades, outcome.max_batch
            );
        }
        let results = [
            bench("full", || run(&tasks, Policy::FullUse).checksum),
            bench("padding", || run(&tasks, Policy::PerTaskPadding).checksum),
            bench("global reserve", || {
                run(&tasks, Policy::GlobalReserve).checksum
            }),
            bench("completion reserve", || {
                run(&tasks, Policy::CompletionReserve).checksum
            }),
        ];
        check_consistency(&results);
        println!();
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Steady => "small finalization bursts",
        Shape::Synchronized => "synchronized large finalization",
        Shape::CdcCommit => "oversized protected CDC commits",
        Shape::Mixed => "mixed analytical and CDC bursts",
    }
}

fn tasks(shape: Shape) -> Vec<Task> {
    let mut random = Lcg::new(0x61C0_0063_u64);
    (0..TASKS)
        .map(|id| {
            let protected = matches!(shape, Shape::CdcCommit | Shape::Mixed) && id % 23 == 0;
            let burst = match shape {
                Shape::Steady => 20 + random.below(21) as u32,
                Shape::Synchronized => 100 + random.below(61) as u32,
                Shape::CdcCommit if protected => 240 + random.below(81) as u32,
                Shape::Mixed if protected => 180 + random.below(81) as u32,
                Shape::CdcCommit | Shape::Mixed => 60 + random.below(81) as u32,
            };
            Task {
                id: id as u64,
                base: 80 + random.below(101) as u32,
                burst,
                work: 40 + random.below(121) as u32,
                protected,
            }
        })
        .collect()
}

fn run(tasks: &[Task], policy: Policy) -> Outcome {
    let mut pending = VecDeque::from(tasks.to_vec());
    let mut completed = Vec::with_capacity(tasks.len());
    let mut makespan = 0_u64;
    let mut spill = 0_u64;
    let mut cascades = 0_u64;
    let mut max_batch = 0_usize;

    while !pending.is_empty() {
        let normal_cap = match policy {
            Policy::FullUse => CAP,
            Policy::PerTaskPadding => CAP,
            Policy::GlobalReserve | Policy::CompletionReserve => CAP - RESERVE,
        };
        let mut admitted = Vec::new();
        let mut used = 0_u32;
        while let Some(task) = pending.front().copied() {
            let charge = match policy {
                Policy::PerTaskPadding => task.base + task.burst,
                Policy::CompletionReserve if task.protected => {
                    task.base + task.burst.saturating_sub(RESERVE)
                }
                _ => task.base,
            };
            if used + charge > normal_cap && !admitted.is_empty() {
                break;
            }
            pending.pop_front();
            used += charge;
            admitted.push(task);
        }
        max_batch = max_batch.max(admitted.len());
        makespan += u64::from(admitted.iter().map(|task| task.work).max().expect("batch"));

        match policy {
            Policy::FullUse => {
                let free = CAP.saturating_sub(admitted.iter().map(|task| task.base).sum::<u32>());
                let total_burst = admitted.iter().map(|task| task.burst).sum::<u32>();
                if total_burst > free {
                    let excess = total_burst - free;
                    spill += u64::from(excess);
                    makespan += u64::from(excess) * 2;
                    cascades += 1;
                }
            }
            Policy::PerTaskPadding => {}
            Policy::GlobalReserve => {
                let largest = admitted.iter().map(|task| task.burst).max().expect("batch");
                if largest > RESERVE {
                    let excess = largest - RESERVE;
                    spill += u64::from(excess);
                    makespan += u64::from(excess) * 2;
                    cascades += 1;
                }
            }
            Policy::CompletionReserve => {
                // Complete a fitting task that releases the most base memory;
                // its release mobilizes the next completion. Protected tasks
                // reserved burst bytes at admission, outside new-query reach.
                let mut mobilized = RESERVE;
                let mut remaining = admitted.clone();
                while !remaining.is_empty() {
                    let candidate = remaining
                        .iter()
                        .enumerate()
                        .filter(|(_, task)| task.protected || task.burst <= mobilized)
                        .max_by_key(|(_, task)| task.base)
                        .map(|(index, _)| index)
                        .unwrap_or_else(|| {
                            remaining
                                .iter()
                                .enumerate()
                                .min_by_key(|(_, task)| task.burst.saturating_sub(mobilized))
                                .map(|(index, _)| index)
                                .expect("remaining task")
                        });
                    let task = remaining.swap_remove(candidate);
                    let available = if task.protected {
                        task.burst.max(mobilized)
                    } else {
                        mobilized
                    };
                    if task.burst > available {
                        let excess = task.burst - available;
                        spill += u64::from(excess);
                        makespan += u64::from(excess) * 2;
                        cascades += 1;
                    }
                    mobilized = (mobilized + task.base).min(CAP);
                }
            }
        }
        for task in admitted {
            completed.push(task.id);
        }
    }
    completed.sort_unstable();
    let checksum = completed
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |checksum, id| {
            (checksum ^ id).wrapping_mul(0x100_0000_01b3)
        });
    assert_eq!(completed.len(), tasks.len());
    Outcome {
        checksum,
        makespan,
        spill,
        cascades,
        max_batch,
    }
}
