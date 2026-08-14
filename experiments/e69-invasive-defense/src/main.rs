//! Prototype e69: throttle persistently harmful templates while preserving a minimum share.

use std::collections::VecDeque;

use common::{Lcg, bench};

const ARRIVAL_TICKS: u64 = 600;
const CAPACITY: u64 = 100;

#[derive(Clone, Copy)]
enum Shape {
    CheapFlood,
    PollutingScan,
    FlashCrowd,
}
#[derive(Clone, Copy)]
enum Policy {
    Fifo,
    TenantQuota,
    TemplateQuota,
    Defense,
}
#[derive(Clone, Copy)]
struct Job {
    arrival: u64,
    cost: u8,
    token: u64,
}
struct Outcome {
    checksum: u64,
    non_invasive_p99: u64,
    useful_throughput: f64,
    invasive_share: f64,
    flash_misclassified: f64,
    max_queue: usize,
}

fn main() {
    println!("e69 — invasive-template resource defense (executable prototype, audited)");
    for shape in [Shape::CheapFlood, Shape::PollutingScan, Shape::FlashCrowd] {
        let arrivals = fixture(shape);
        let policies = [
            ("FIFO", Policy::Fifo),
            ("tenant quota", Policy::TenantQuota),
            ("template quota", Policy::TemplateQuota),
            ("harm feedback", Policy::Defense),
        ];
        let outcomes = policies.map(|(_, policy)| execute(&arrivals, policy, shape));
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.checksum == outcomes[0].checksum)
        );
        println!("\n=== {} ===", shape_name(shape));
        for ((name, _), out) in policies.iter().zip(outcomes.iter()) {
            println!(
                "{name:<16} non-invasive p99 {:>5} useful/tick {:>6.1} invasive share {:>5.1}% flash misclass {:>5.1}% max queue {}",
                out.non_invasive_p99,
                out.useful_throughput,
                out.invasive_share * 100.0,
                out.flash_misclassified * 100.0,
                out.max_queue
            );
        }
        for (name, policy) in policies {
            let _ = bench(name, || execute(&arrivals, policy, shape).checksum);
        }
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::CheapFlood => "cheap flood",
        Shape::PollutingScan => "cache-polluting scan",
        Shape::FlashCrowd => "legitimate flash crowd",
    }
}

fn fixture(shape: Shape) -> Vec<(Vec<Job>, Vec<Job>)> {
    let mut random = Lcg::new(0x6900_0069 ^ shape as u64);
    (0..ARRIVAL_TICKS)
        .map(|tick| {
            let invasive = match shape {
                Shape::CheapFlood => 100,
                Shape::PollutingScan => 20,
                Shape::FlashCrowd if (200..240).contains(&tick) => 100,
                Shape::FlashCrowd => 10,
            };
            let cost = if matches!(shape, Shape::PollutingScan) {
                4
            } else {
                1
            };
            let invasive = (0..invasive)
                .map(|_| Job {
                    arrival: tick,
                    cost,
                    token: random.next_u64(),
                })
                .collect();
            let diverse = (0..40)
                .map(|_| Job {
                    arrival: tick,
                    cost: 1,
                    token: random.next_u64(),
                })
                .collect();
            (invasive, diverse)
        })
        .collect()
}

fn execute(arrivals: &[(Vec<Job>, Vec<Job>)], policy: Policy, shape: Shape) -> Outcome {
    let mut invasive = VecDeque::new();
    let mut diverse = VecDeque::new();
    let mut diverse_latencies = Vec::new();
    let mut tick = 0_u64;
    let mut persistent_harm = 0_u64;
    let mut flash_capped = 0_u64;
    let mut flash_ticks = 0_u64;
    let mut arrival_work_done = 0_u64;
    let mut minimum_contended_share = 1.0_f64;
    let mut max_queue = 0;
    let mut count = 0_u64;
    let mut sum = 0_u64;
    let mut xor = 0_u64;
    while tick < ARRIVAL_TICKS || !invasive.is_empty() || !diverse.is_empty() {
        if tick < ARRIVAL_TICKS {
            let (new_invasive, new_diverse) = &arrivals[tick as usize];
            invasive.extend(new_invasive.iter().copied());
            diverse.extend(new_diverse.iter().copied());
        }
        max_queue = max_queue.max(invasive.len() + diverse.len());
        let diverse_wait = diverse
            .front()
            .map_or(0, |job| tick.saturating_sub(job.arrival));
        let harmful = diverse_wait > 20
            || invasive.len() > diverse.len().saturating_mul(2)
            || invasive.front().is_some_and(|job| job.cost > 1);
        persistent_harm = if harmful {
            persistent_harm + 1
        } else {
            persistent_harm.saturating_sub(2)
        };
        let invasive_cap = match policy {
            Policy::Fifo => CAPACITY,
            Policy::TenantQuota => 50,
            Policy::TemplateQuota => 25,
            Policy::Defense if persistent_harm >= 50 => 20,
            Policy::Defense => CAPACITY,
        };
        if matches!(shape, Shape::FlashCrowd) && (200..240).contains(&tick) {
            flash_ticks += 1;
            if invasive_cap < CAPACITY {
                flash_capped += 1;
            }
        }
        let mut budget = CAPACITY;
        let mut used_invasive = 0_u64;
        let contended = !invasive.is_empty() && !diverse.is_empty();
        while budget > 0 && (!invasive.is_empty() || !diverse.is_empty()) {
            let take_invasive = if diverse.is_empty() {
                true
            } else if invasive.is_empty() || used_invasive >= invasive_cap {
                false
            } else {
                match policy {
                    Policy::Fifo => {
                        invasive.front().unwrap().arrival <= diverse.front().unwrap().arrival
                    }
                    Policy::TenantQuota | Policy::TemplateQuota | Policy::Defense => {
                        used_invasive < invasive_cap
                    }
                }
            };
            let queue = if take_invasive {
                &mut invasive
            } else {
                &mut diverse
            };
            let mut job = queue.pop_front().unwrap();
            let work = budget.min(u64::from(job.cost));
            budget -= work;
            if tick < ARRIVAL_TICKS {
                arrival_work_done += work;
            }
            if take_invasive {
                used_invasive += work;
            }
            job.cost -= work as u8;
            if job.cost > 0 {
                queue.push_front(job);
            } else {
                count += 1;
                sum = sum.wrapping_add(job.token);
                xor ^= job.token;
                if !take_invasive {
                    diverse_latencies.push(tick + 1 - job.arrival);
                }
            }
        }
        let used = CAPACITY - budget;
        if contended && used > 0 {
            minimum_contended_share =
                minimum_contended_share.min(used_invasive as f64 / used as f64);
        }
        tick += 1;
    }
    diverse_latencies.sort_unstable();
    let checksum = count.rotate_left(7) ^ sum.rotate_left(19) ^ xor;
    Outcome {
        checksum,
        non_invasive_p99: diverse_latencies[diverse_latencies.len() * 99 / 100],
        useful_throughput: arrival_work_done as f64 / ARRIVAL_TICKS as f64,
        invasive_share: minimum_contended_share,
        flash_misclassified: if flash_ticks == 0 {
            0.0
        } else {
            flash_capped as f64 / flash_ticks as f64
        },
        max_queue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_policy_completes_the_exact_same_jobs() {
        for shape in [Shape::CheapFlood, Shape::PollutingScan, Shape::FlashCrowd] {
            let arrivals = fixture(shape);
            let expected = execute(&arrivals, Policy::Fifo, shape).checksum;
            for policy in [Policy::TenantQuota, Policy::TemplateQuota, Policy::Defense] {
                assert_eq!(execute(&arrivals, policy, shape).checksum, expected);
            }
        }
    }
    #[test]
    fn defense_preserves_minimum_share_and_avoids_flash_misclassification() {
        let flash = fixture(Shape::FlashCrowd);
        let out = execute(&flash, Policy::Defense, Shape::FlashCrowd);
        assert!(out.invasive_share >= 0.05);
        assert!(out.flash_misclassified < 0.01);
    }
}
