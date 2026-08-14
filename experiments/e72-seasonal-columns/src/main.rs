//! e72: migrate decoded column mirrors only after recurring demand repays migration.

use common::{Lcg, bench, check_consistency};

const COLUMNS: usize = 24;
const CAPACITY: usize = 8;
const QUERIES: usize = 30_000;
const COMPRESSED_COST: u64 = 28;
const DECODED_COST: u64 = 5;
const MIGRATION_COST: u64 = 180;

#[derive(Clone, Copy)]
enum Shape {
    Periodic,
    Drift,
    Random,
    Wide,
}

#[derive(Clone, Copy)]
enum Policy {
    Compressed,
    FixedDecoded,
    Recency,
    Seasonal,
}

#[derive(Clone, Copy)]
struct Query {
    columns: u32,
    values: [u32; COLUMNS],
}

struct Outcome {
    checksum: u64,
    latency: u64,
    migrations: u64,
    peak: usize,
}

fn main() {
    println!("e72 — seasonal decoded-column migration (simulation tier, audited)");
    for shape in [Shape::Periodic, Shape::Drift, Shape::Random, Shape::Wide] {
        let trace = trace(shape);
        println!("\n=== {} ===", shape_name(shape));
        let policies = [
            ("always compressed", Policy::Compressed),
            ("fixed decoded", Policy::FixedDecoded),
            ("recency", Policy::Recency),
            ("seasonal migration", Policy::Seasonal),
        ];
        let mut expected = None;
        for (name, policy) in policies {
            let outcome = run(&trace, policy);
            expected.map_or_else(
                || expected = Some(outcome.checksum),
                |checksum| assert_eq!(checksum, outcome.checksum),
            );
            println!(
                "{name:<22} latency {:>10} migrations {:>6} peak {}/{}",
                outcome.latency, outcome.migrations, outcome.peak, CAPACITY,
            );
        }
        let results = policies.map(|(name, policy)| bench(name, || run(&trace, policy).checksum));
        check_consistency(&results);
    }
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Periodic => "periodic dashboards + one-off probes",
        Shape::Drift => "drifting season length",
        Shape::Random => "random access",
        Shape::Wide => "wide-column control",
    }
}

fn mask(columns: impl IntoIterator<Item = usize>) -> u32 {
    columns
        .into_iter()
        .fold(0_u32, |bits, column| bits | (1_u32 << column))
}

fn trace(shape: Shape) -> Vec<Query> {
    let mut random = Lcg::new(0x7200_0072);
    (0..QUERIES)
        .map(|tick| {
            let columns = match shape {
                Shape::Periodic => seasonal_mask(tick, 200, &mut random),
                Shape::Drift => {
                    let season = if tick < QUERIES / 2 { 180 } else { 260 };
                    seasonal_mask(tick, season, &mut random)
                }
                Shape::Random => {
                    let first = random.below(COLUMNS as u64) as usize;
                    let second = random.below(COLUMNS as u64) as usize;
                    mask([first, second])
                }
                Shape::Wide => mask(0..12),
            };
            let values = std::array::from_fn(|column| {
                (tick as u32).wrapping_mul(31) ^ (column as u32).wrapping_mul(0x9e37)
            });
            Query { columns, values }
        })
        .collect()
}

fn seasonal_mask(tick: usize, season_length: usize, random: &mut Lcg) -> u32 {
    let season = (tick / season_length) % 3;
    let base = season * CAPACITY;
    let mut columns = mask(base..base + 4);
    // Sparse exploratory columns deliberately tempt a pure recency policy.
    if tick.is_multiple_of(10) {
        columns |= 1_u32 << random.below(COLUMNS as u64);
    }
    columns
}

fn run(trace: &[Query], policy: Policy) -> Outcome {
    let mut hot = [false; COLUMNS];
    let mut last = [None; COLUMNS];
    let mut period = [None; COLUMNS];
    let mut stable_runs = [0_u8; COLUMNS];
    let mut observations = [0_u32; COLUMNS];
    let mut latency = 0_u64;
    let mut migrations = 0_u64;
    let mut peak = 0_usize;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;

    if matches!(policy, Policy::FixedDecoded) {
        hot.iter_mut()
            .take(CAPACITY)
            .for_each(|value| *value = true);
        migrations = CAPACITY as u64;
        latency += MIGRATION_COST * CAPACITY as u64;
    }

    for (tick, query) in trace.iter().enumerate() {
        if matches!(policy, Policy::Seasonal) {
            preposition(
                tick,
                &mut hot,
                &last,
                &period,
                &stable_runs,
                &observations,
                &mut latency,
                &mut migrations,
            );
        }

        let mut answer = 0_u64;
        for column in 0..COLUMNS {
            if query.columns & (1_u32 << column) == 0 {
                continue;
            }
            if matches!(policy, Policy::Recency) && !hot[column] {
                admit(column, tick, &mut hot, &last);
                latency += MIGRATION_COST;
                migrations += 1;
            }
            latency += if hot[column] {
                DECODED_COST
            } else {
                COMPRESSED_COST
            };
            answer = answer
                .rotate_left(7)
                .wrapping_add(u64::from(query.values[column]));

            if let Some(previous) = last[column] {
                let gap = tick - previous;
                match period[column] {
                    Some(estimate) if gap.abs_diff(estimate) <= (estimate / 10).max(1) => {
                        stable_runs[column] = stable_runs[column].saturating_add(1);
                        period[column] = Some((estimate * 3 + gap) / 4);
                    }
                    _ => {
                        stable_runs[column] = 0;
                        period[column] = Some(gap);
                    }
                }
            }
            last[column] = Some(tick);
            observations[column] += 1;
        }
        peak = peak.max(hot.iter().filter(|value| **value).count());
        checksum = (checksum ^ answer).wrapping_mul(0x100_0000_01b3);
    }

    Outcome {
        checksum,
        latency,
        migrations,
        peak,
    }
}

#[allow(clippy::too_many_arguments)]
fn preposition(
    tick: usize,
    hot: &mut [bool; COLUMNS],
    last: &[Option<usize>; COLUMNS],
    period: &[Option<usize>; COLUMNS],
    stable_runs: &[u8; COLUMNS],
    observations: &[u32; COLUMNS],
    latency: &mut u64,
    migrations: &mut u64,
) {
    let mut eligible = Vec::new();
    for column in 0..COLUMNS {
        let (Some(previous), Some(estimate)) = (last[column], period[column]) else {
            continue;
        };
        let due = previous.saturating_add(estimate);
        let projected_uses = 16_u64;
        let repayable = projected_uses * (COMPRESSED_COST - DECODED_COST) > MIGRATION_COST;
        if stable_runs[column] >= 6
            && observations[column] >= 8
            && tick >= due.saturating_sub(1)
            && tick <= due.saturating_add(1)
            && repayable
        {
            eligible.push(column);
        }
    }

    eligible.sort_unstable_by(|left, right| {
        period[*left]
            .cmp(&period[*right])
            .then_with(|| stable_runs[*right].cmp(&stable_runs[*left]))
            .then_with(|| observations[*right].cmp(&observations[*left]))
            .then_with(|| left.cmp(right))
    });
    eligible.truncate(CAPACITY);

    let mut desired = [false; COLUMNS];
    for column in eligible {
        desired[column] = true;
    }
    for column in 0..COLUMNS {
        if !desired[column] || hot[column] {
            continue;
        }
        if hot.iter().filter(|value| **value).count() == CAPACITY {
            let victim = (0..COLUMNS)
                .filter(|candidate| hot[*candidate] && !desired[*candidate])
                .max_by_key(|candidate| tick.saturating_sub(last[*candidate].unwrap_or(0)));
            let Some(victim) = victim else {
                continue;
            };
            hot[victim] = false;
        }
        hot[column] = true;
        *latency += MIGRATION_COST;
        *migrations += 1;
    }
}

fn admit(column: usize, tick: usize, hot: &mut [bool; COLUMNS], last: &[Option<usize>; COLUMNS]) {
    if hot.iter().filter(|value| **value).count() == CAPACITY {
        let victim = (0..COLUMNS)
            .filter(|candidate| hot[*candidate])
            .max_by_key(|candidate| tick.saturating_sub(last[*candidate].unwrap_or(0)))
            .expect("a full mirror set has a victim");
        hot[victim] = false;
    }
    hot[column] = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_twenty_four_columns_have_distinct_bits() {
        let all = mask(0..COLUMNS);
        assert_eq!(all.count_ones(), COLUMNS as u32);
        for column in 0..COLUMNS {
            assert_ne!(all & (1_u32 << column), 0);
        }
    }

    #[test]
    fn every_policy_computes_the_same_query_answers() {
        let trace = trace(Shape::Periodic);
        let expected = run(&trace, Policy::Compressed).checksum;
        for policy in [Policy::FixedDecoded, Policy::Recency, Policy::Seasonal] {
            assert_eq!(run(&trace, policy).checksum, expected);
        }
    }

    #[test]
    fn random_access_does_not_trigger_false_seasonal_migration() {
        let outcome = run(&trace(Shape::Random), Policy::Seasonal);
        assert!(outcome.migrations < 10, "{} migrations", outcome.migrations);
    }

    #[test]
    fn stable_periodic_demand_repays_migration() {
        let trace = trace(Shape::Periodic);
        let compressed = run(&trace, Policy::Compressed);
        let seasonal = run(&trace, Policy::Seasonal);
        assert!(seasonal.latency < compressed.latency);
        let season_transitions = QUERIES.div_ceil(200) as u64;
        assert!(
            seasonal.migrations <= season_transitions * 4,
            "{} migrations for {season_transitions} active-set seasons",
            seasonal.migrations,
        );
    }

    #[test]
    fn wide_demand_does_not_churn_the_capacity_limited_mirror() {
        let outcome = run(&trace(Shape::Wide), Policy::Seasonal);
        assert_eq!(outcome.migrations, CAPACITY as u64);
        assert_eq!(outcome.peak, CAPACITY);
    }
}
