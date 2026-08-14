//! Prototype e61: contain runaway queries using only observable independent gates.

use common::{Lcg, bench};

#[derive(Clone, Copy, Eq, PartialEq)]
enum Truth {
    Healthy,
    LegitimatelySlow,
    Cartesian,
    SkewExplosion,
    RecoverableSpill,
}

#[derive(Clone, Copy)]
enum Policy {
    Timeout,
    MemoryCap,
    ProgressOnly,
    Consensus,
}

#[derive(Clone, Copy)]
struct Observation {
    elapsed: u16,
    memory_pressure: u16,
    progress_per_tick: u16,
    estimated_remaining: u32,
    slo_damage: u16,
    spill_relief: u16,
}

struct QueryCase {
    truth: Truth,
    token: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Continue,
    Spill,
    Abort,
}

struct Outcome {
    checksum: u64,
    doomed_stopped: u64,
    doomed_total: u64,
    mean_doomed_consumption: f64,
    false_aborts: u64,
    protected_total: u64,
    recoverable_preserved: u64,
    healthy_p99: u64,
}

fn main() {
    println!("e61 — runaway-query consensus containment (executable prototype, audited)");
    let cases = cases();
    let policies = [
        ("timeout", Policy::Timeout),
        ("memory cap", Policy::MemoryCap),
        ("progress only", Policy::ProgressOnly),
        ("four-signal consensus", Policy::Consensus),
    ];
    for (name, policy) in policies {
        let outcome = evaluate(&cases, policy);
        println!(
            "{name:<22} stopped {}/{} mean consumption {:>5.1}% false abort {}/{} spill preserved {} healthy p99 {}",
            outcome.doomed_stopped,
            outcome.doomed_total,
            outcome.mean_doomed_consumption * 100.0,
            outcome.false_aborts,
            outcome.protected_total,
            outcome.recoverable_preserved,
            outcome.healthy_p99,
        );
    }
    for (name, policy) in policies {
        let _ = bench(name, || evaluate(&cases, policy).checksum);
    }
}

fn cases() -> Vec<QueryCase> {
    let mut random = Lcg::new(0x6100_0061);
    let mut cases = Vec::new();
    for (truth, count) in [
        (Truth::Healthy, 10_000),
        (Truth::LegitimatelySlow, 1_000),
        (Truth::Cartesian, 700),
        (Truth::SkewExplosion, 300),
        (Truth::RecoverableSpill, 1_000),
    ] {
        for _ in 0..count {
            cases.push(QueryCase {
                truth,
                token: random.next_u64(),
            });
        }
    }
    cases
}

fn observe(case: &QueryCase, tick: u16) -> Observation {
    match case.truth {
        Truth::Healthy => Observation {
            elapsed: tick,
            memory_pressure: 20 + tick / 10,
            progress_per_tick: 12,
            estimated_remaining: u32::from(100_u16.saturating_sub(tick)) * 12,
            slo_damage: tick / 8,
            spill_relief: 0,
        },
        Truth::LegitimatelySlow => Observation {
            elapsed: tick,
            memory_pressure: 30,
            progress_per_tick: 1,
            estimated_remaining: u32::from(100_u16.saturating_sub(tick)) * 40,
            slo_damage: tick,
            spill_relief: 10,
        },
        Truth::Cartesian => Observation {
            elapsed: tick,
            memory_pressure: 50 + tick,
            progress_per_tick: 0,
            estimated_remaining: 100_000 - u32::from(tick) * 10,
            slo_damage: tick * 3,
            spill_relief: 5,
        },
        Truth::SkewExplosion => Observation {
            elapsed: tick,
            memory_pressure: 55 + tick,
            progress_per_tick: 1,
            estimated_remaining: 80_000 - u32::from(tick) * 5,
            slo_damage: tick * 3,
            spill_relief: 15,
        },
        Truth::RecoverableSpill => Observation {
            elapsed: tick,
            memory_pressure: 50 + tick,
            progress_per_tick: 1,
            estimated_remaining: 4_000 - u32::from(tick) * 20,
            slo_damage: tick * 2,
            spill_relief: 90,
        },
    }
}

fn decide(observation: &Observation, policy: Policy) -> Action {
    match policy {
        Policy::Timeout if observation.elapsed >= 70 => Action::Abort,
        Policy::MemoryCap if observation.memory_pressure >= 90 => Action::Abort,
        Policy::ProgressOnly if observation.elapsed >= 30 && observation.progress_per_tick <= 1 => {
            Action::Abort
        }
        Policy::Consensus
            if observation.memory_pressure >= 65 && observation.spill_relief >= 70 =>
        {
            Action::Spill
        }
        Policy::Consensus => {
            let votes = u8::from(observation.memory_pressure >= 65)
                + u8::from(observation.progress_per_tick <= 1)
                + u8::from(observation.estimated_remaining >= 2_000)
                + u8::from(observation.slo_damage >= 30);
            if votes == 4 {
                Action::Abort
            } else {
                Action::Continue
            }
        }
        Policy::Timeout | Policy::MemoryCap | Policy::ProgressOnly => Action::Continue,
    }
}

fn answer(case: &QueryCase) -> u64 {
    case.token
        .rotate_left(17)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

fn is_doomed(truth: Truth) -> bool {
    matches!(truth, Truth::Cartesian | Truth::SkewExplosion)
}

fn completion_tick(truth: Truth) -> u16 {
    match truth {
        Truth::Healthy => 20,
        Truth::LegitimatelySlow => 95,
        Truth::RecoverableSpill => 90,
        Truth::Cartesian | Truth::SkewExplosion => 100,
    }
}

fn evaluate(cases: &[QueryCase], policy: Policy) -> Outcome {
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut doomed_stopped = 0_u64;
    let mut doomed_total = 0_u64;
    let mut doomed_consumption = 0_u64;
    let mut false_aborts = 0_u64;
    let mut protected_total = 0_u64;
    let mut recoverable_preserved = 0_u64;

    for case in cases {
        if is_doomed(case.truth) {
            doomed_total += 1;
        } else {
            protected_total += 1;
        }
        let mut final_action = Action::Continue;
        let mut consumed = 100_u16;
        for tick in 0..100_u16 {
            if tick == completion_tick(case.truth) {
                consumed = tick;
                break;
            }
            let action = decide(&observe(case, tick), policy);
            if action != Action::Continue {
                final_action = action;
                consumed = tick;
                break;
            }
        }

        if is_doomed(case.truth) {
            doomed_consumption += u64::from(consumed);
            if final_action == Action::Abort && consumed < 30 {
                doomed_stopped += 1;
            }
        } else if final_action == Action::Abort {
            false_aborts += 1;
        } else {
            checksum = checksum.rotate_left(7) ^ answer(case);
            if case.truth == Truth::RecoverableSpill {
                recoverable_preserved += 1;
            }
        }
    }

    let attack_penalty = doomed_consumption / doomed_total.max(1) * 8;
    let mut healthy_latencies = cases
        .iter()
        .filter(|case| case.truth == Truth::Healthy)
        .map(|case| 100 + case.token % 41 + attack_penalty)
        .collect::<Vec<_>>();
    healthy_latencies.sort_unstable();
    let healthy_p99 = healthy_latencies[healthy_latencies.len() * 99 / 100];
    Outcome {
        checksum,
        doomed_stopped,
        doomed_total,
        mean_doomed_consumption: doomed_consumption as f64 / (doomed_total.max(1) * 100) as f64,
        false_aborts,
        protected_total,
        recoverable_preserved,
        healthy_p99,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_accepts_observations_not_oracle_truth() {
        let case = QueryCase {
            truth: Truth::Cartesian,
            token: 7,
        };
        let observation = observe(&case, 20);
        assert_eq!(decide(&observation, Policy::Consensus), Action::Abort);
    }

    #[test]
    fn consensus_preserves_every_non_doomed_answer() {
        let cases = cases();
        let outcome = evaluate(&cases, Policy::Consensus);
        assert_eq!(outcome.false_aborts, 0);
        assert_eq!(outcome.recoverable_preserved, 1_000);
        let expected = cases
            .iter()
            .filter(|case| !is_doomed(case.truth))
            .fold(0xcbf2_9ce4_8422_2325_u64, |checksum, case| {
                checksum.rotate_left(7) ^ answer(case)
            });
        assert_eq!(outcome.checksum, expected);
    }

    #[test]
    fn consensus_stops_doomed_queries_before_thirty_percent() {
        let outcome = evaluate(&cases(), Policy::Consensus);
        assert_eq!(outcome.doomed_stopped, outcome.doomed_total);
        assert!(outcome.mean_doomed_consumption < 0.30);
    }
}
