//! e62: the speed of light for Q5, and how much of the engine's time is
//! machinery rather than work.
//!
//! Every optimisation this session has been incremental because there was no
//! reference point: 159ms was compared only against ClickHouse's 40ms, which
//! says a gap exists but not where it is or how much is reclaimable. This
//! measures the floor directly - a hand-written loop doing exactly what Q5
//! needs and nothing else, on the same host, over the same data shape.
//!
//! Q5 is:
//!   SELECT YEAR(order_date), MONTH(order_date), COUNT(*), SUM(total_amount)
//!   FROM orders WHERE order_date >= '2023-01-01' AND order_date < '2024-01-01'
//!   GROUP BY 1, 2
//!
//! The arms climb from that floor toward the engine, adding one layer of real
//! work at a time, so the cost of each layer is attributable rather than
//! inferred:
//!
//!   1. floor           - native-width columns, precomputed group ordinals
//!   2. date arithmetic - derive year and month per row from day units
//!   3. i64 columns     - the widths the engine actually stores
//!   4. batched         - the same work split into 65k batches with per-batch
//!                        setup, approximating the executor's granularity
//!
//! The engine's measured Q5 on this host is 159ms at 16 threads, with a
//! per-phase split of decode 59, slicing 26, ingest 29, drain 20, adopt 14.
//! Whatever arm 4 costs, the difference is machinery: dispatch, accounting,
//! allocation and the operator model.

use common::bench;
use rayon::prelude::*;

const ROWS: usize = 20_000_000;
const MORSEL: usize = 65_536;
/// Days since the epoch for 2023-01-01 and 2024-01-01.
const WINDOW_START: i32 = 19_358;
const WINDOW_END: i32 = 19_723;
/// Twelve months of one year, matching Q5's answer.
const GROUPS: usize = 12;

/// The engine's seed formula: `MOD(generated_id * 7, 1825)` days from
/// 2020-01-01, so dates are scattered across five years and every block spans
/// the whole range. Amounts are cents, which is what a DECIMAL(10,2) holds.
struct Data {
    /// Day units, as `Date32` stores them.
    days: Vec<i32>,
    /// Scaled decimal units, as the store emits them.
    cents: Vec<i64>,
    /// Precomputed month ordinal, for the floor arm only.
    group: Vec<u8>,
}

fn generate() -> Data {
    let base = 18_262_i32; // 2020-01-01
    let mut days = Vec::with_capacity(ROWS);
    let mut cents = Vec::with_capacity(ROWS);
    let mut group = Vec::with_capacity(ROWS);
    for row in 0..ROWS {
        let offset = i32::try_from((row as u64 * 7) % 1825).expect("offset fits i32");
        let day = base + offset;
        days.push(day);
        let quantity = 1 + (row as i64) % 20;
        cents.push(quantity * (1_000 + (row as i64 * 7919) % 99_000));
        group.push(u8::try_from(civil_month(day) - 1).expect("month fits u8"));
    }
    Data { days, cents, group }
}

/// Hinnant's civil-from-days, the same arithmetic the engine uses.
fn civil(days: i32) -> (i32, i32) {
    let shifted = i64::from(days) + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_shifted = (5 * day_of_year + 2) / 153;
    let month = if month_shifted < 10 {
        month_shifted + 3
    } else {
        month_shifted - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        i32::try_from(year).expect("year fits i32"),
        i32::try_from(month).expect("month fits i32"),
    )
}

fn civil_month(days: i32) -> i32 {
    civil(days).1
}

type Totals = ([u64; GROUPS], [i64; GROUPS]);

fn merge(mut left: Totals, right: Totals) -> Totals {
    for slot in 0..GROUPS {
        left.0[slot] = left.0[slot].wrapping_add(right.0[slot]);
        left.1[slot] = left.1[slot].wrapping_add(right.1[slot]);
    }
    left
}

fn checksum(totals: &Totals) -> u64 {
    (0..GROUPS).fold(0_u64, |acc, slot| {
        acc.wrapping_mul(31)
            .wrapping_add(totals.0[slot])
            .wrapping_mul(31)
            .wrapping_add(totals.1[slot] as u64)
    })
}

/// Arm 1: the floor. Native widths, group ordinal already known, filter as a
/// pair of integer comparisons. This is the least work the query can be.
fn arm_floor(data: &Data) -> u64 {
    let totals = (0..ROWS)
        .into_par_iter()
        .with_min_len(MORSEL)
        .fold(
            || ([0_u64; GROUPS], [0_i64; GROUPS]),
            |mut totals, row| {
                let day = data.days[row];
                if day >= WINDOW_START && day < WINDOW_END {
                    let slot = usize::from(data.group[row]);
                    totals.0[slot] += 1;
                    totals.1[slot] += data.cents[row];
                }
                totals
            },
        )
        .reduce(|| ([0_u64; GROUPS], [0_i64; GROUPS]), merge);
    checksum(&totals)
}

/// Arm 2: derive the group from the date, as the query really must.
fn arm_date_arithmetic(data: &Data) -> u64 {
    let totals = (0..ROWS)
        .into_par_iter()
        .with_min_len(MORSEL)
        .fold(
            || ([0_u64; GROUPS], [0_i64; GROUPS]),
            |mut totals, row| {
                let day = data.days[row];
                if day >= WINDOW_START && day < WINDOW_END {
                    let (_year, month) = civil(day);
                    let slot = usize::try_from(month - 1).expect("month fits usize");
                    totals.0[slot] += 1;
                    totals.1[slot] += data.cents[row];
                }
                totals
            },
        )
        .reduce(|| ([0_u64; GROUPS], [0_i64; GROUPS]), merge);
    checksum(&totals)
}

/// Arm 3: the widths the engine actually materialises - days as i64 rather
/// than i32, which doubles the date column's bytes.
fn arm_i64_columns(days64: &[i64], data: &Data) -> u64 {
    let totals = (0..ROWS)
        .into_par_iter()
        .with_min_len(MORSEL)
        .fold(
            || ([0_u64; GROUPS], [0_i64; GROUPS]),
            |mut totals, row| {
                let day = i32::try_from(days64[row]).expect("day fits i32");
                if day >= WINDOW_START && day < WINDOW_END {
                    let (_year, month) = civil(day);
                    let slot = usize::try_from(month - 1).expect("month fits usize");
                    totals.0[slot] += 1;
                    totals.1[slot] += data.cents[row];
                }
                totals
            },
        )
        .reduce(|| ([0_u64; GROUPS], [0_i64; GROUPS]), merge);
    checksum(&totals)
}

/// Arm 4: the same work at the executor's granularity - 65k batches, each with
/// its own accumulator allocated and merged, approximating per-batch setup.
fn arm_batched(days64: &[i64], data: &Data) -> u64 {
    let totals = (0..ROWS.div_ceil(MORSEL))
        .into_par_iter()
        .fold(
            || ([0_u64; GROUPS], [0_i64; GROUPS]),
            |totals, batch| {
                let start = batch * MORSEL;
                let end = (start + MORSEL).min(ROWS);
                // Per-batch accumulator, as an operator would build.
                let mut local = ([0_u64; GROUPS], [0_i64; GROUPS]);
                for row in start..end {
                    let day = i32::try_from(days64[row]).expect("day fits i32");
                    if day >= WINDOW_START && day < WINDOW_END {
                        let (_year, month) = civil(day);
                        let slot = usize::try_from(month - 1).expect("month fits usize");
                        local.0[slot] += 1;
                        local.1[slot] += data.cents[row];
                    }
                }
                merge(totals, local)
            },
        )
        .reduce(|| ([0_u64; GROUPS], [0_i64; GROUPS]), merge);
    checksum(&totals)
}

fn main() {
    let threads = rayon::current_num_threads();
    println!("e62: Q5 speed of light | {ROWS} rows, {threads} rayon threads");
    println!("engine measures 159ms for this query at 16 threads (decode 59, slicing 26, ingest 29, drain 20, adopt 14)\n");

    let data = generate();
    let days64: Vec<i64> = data.days.iter().map(|day| i64::from(*day)).collect();

    let results = vec![
        bench("1. floor (i32 date, group known)", || arm_floor(&data)),
        bench("2. + date arithmetic per row", || arm_date_arithmetic(&data)),
        bench("3. + i64 date column", || arm_i64_columns(&days64, &data)),
        bench("4. + 65k batch granularity", || arm_batched(&days64, &data)),
    ];

    let floor = results[0].min_ms;
    for result in &results {
        println!(
            "  {:<34} min {:>7.1} ms  median {:>7.1} ms  {:>5.2}x floor  ck {:016x}",
            result.name,
            result.min_ms,
            result.median_ms,
            result.min_ms / floor,
            result.checksum
        );
    }
    let specialised = results[3].min_ms;
    println!(
        "\n  specialised loop doing the query's real work: {specialised:.1} ms"
    );
    println!("  engine: 159.0 ms  =>  {:.1}x of the specialised loop is machinery", 159.0 / specialised);
    println!("  ClickHouse: 40.0 ms  =>  {:.1}x of the specialised loop", 40.0 / specialised);
}
