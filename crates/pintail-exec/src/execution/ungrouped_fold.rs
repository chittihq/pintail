//! Column-at-a-time folds for an aggregate with no GROUP BY.
//!
//! An ungrouped aggregate over a filtered scan - a COUNT and a SUM over the
//! last seven days of an event table, say - went through the grouped row
//! loop with an empty key: per selected row it hashed the empty key, probed
//! the one-entry map, charged the memory tracker and dispatched every
//! aggregate through the general update, and an integer SUM or a temporal
//! MIN materialized its column as `Value` cells to read one number. That
//! measured 80 to 270 ns a row, single-threaded, where the scan beneath it
//! spent 1 to 3 ns. Block skipping had already cut the scan to the window;
//! the aggregate was the whole query.
//!
//! Here each aggregate folds its column over the batch's selected rows in
//! one typed loop and then updates its state once per batch. Every fold is
//! one the per-row update performs exactly: counts, integer and scaled
//! decimal unit totals, and the first row holding a MIN or MAX in row order.
//! An aggregate with no column fold here keeps the per-row update, for its
//! own state only.

use pintail_sql::AggregateFunction;
use pintail_types::{DataType, Value};

use super::aggregate::{
    AggregateState, CompiledAggregate, aggregate_uses_float, decimal_average_scale,
    update_aggregate_states,
};
use super::distinct_keys::UnitKind;
use super::packed_fold::{FoldRows, fold_rows};
use super::{ExecError, MemoryTracker};
use crate::RecordBatch;
use crate::array::{StrColumn, ValidityMask};
use crate::batch::{DecimalUnits, TypedValues};
use crate::collation::Collation;

/// Widest decimal-average widening folded as one partial total: an `i64`
/// unit times `10^19` still fits `i128`.
const AVERAGE_MAX_DIGITS: u8 = 19;

/// Whether every aggregate can take this fold at all. The ones that collect
/// values (`GROUP_CONCAT`, the JSON aggregates) keep the general path, which
/// can spill them.
pub(super) fn eligible(aggregates: &[CompiledAggregate]) -> bool {
    !aggregates.is_empty()
        && aggregates.iter().all(|aggregate| {
            !matches!(
                aggregate.function,
                AggregateFunction::GroupConcat
                    | AggregateFunction::JsonArrayAgg
                    | AggregateFunction::JsonObjectAgg
            )
        })
}

/// Per query: how many aggregate-batches folded by column, and how many
/// fell back to the per-row update. Recorded on the profile.
#[derive(Default)]
pub(super) struct FoldTally {
    pub(super) folded: usize,
    pub(super) per_row: usize,
}

/// Folds `batch`'s selected rows into `states`, one per aggregate.
pub(super) fn fold_batch(
    batch: &RecordBatch,
    aggregates: &[CompiledAggregate],
    states: &mut [AggregateState],
    rows_buffer: &mut Vec<u32>,
    tally: &mut FoldTally,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let rows = fold_rows(batch, 0..batch.row_count(), rows_buffer);
    fold_rows_into(batch, &rows, aggregates, states, tally, memory)
}

/// Folds `batch`'s selected rows into `states` by column alone. `false`
/// when an aggregate has no column fold for this batch: `states` then
/// holds part of the batch and is the caller's to discard.
pub(super) fn fold_batch_by_column(
    batch: &RecordBatch,
    aggregates: &[CompiledAggregate],
    states: &mut [AggregateState],
    rows_buffer: &mut Vec<u32>,
    memory: &MemoryTracker,
) -> Result<bool, ExecError> {
    let rows = fold_rows(batch, 0..batch.row_count(), rows_buffer);
    if rows.len() == 0 {
        return Ok(true);
    }
    for (aggregate, state) in aggregates.iter().zip(states.iter_mut()) {
        if !fold_column(batch, &rows, aggregate, state, memory)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Folds the rows `rows` lists, in row order, into `states`: by column
/// where a fold exists, per row otherwise.
pub(super) fn fold_rows_into(
    batch: &RecordBatch,
    rows: &FoldRows<'_>,
    aggregates: &[CompiledAggregate],
    states: &mut [AggregateState],
    tally: &mut FoldTally,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    if rows.len() == 0 {
        return Ok(());
    }
    let batch_bytes = batch.estimated_bytes();
    for (aggregate, state) in aggregates.iter().zip(states.iter_mut()) {
        if fold_column(batch, rows, aggregate, state, memory)? {
            tally.folded += 1;
            continue;
        }
        tally.per_row += 1;
        let mut update = |row: usize| {
            update_aggregate_states(
                batch,
                row,
                batch_bytes,
                std::slice::from_ref(aggregate),
                std::slice::from_mut(state),
                memory,
            )
        };
        match rows {
            FoldRows::Span(span) => span.clone().try_for_each(&mut update)?,
            FoldRows::Picked(picked) => picked.iter().try_for_each(|row| update(*row as usize))?,
        }
    }
    Ok(())
}

/// The rows of `rows` whose `validity` bit is set.
fn valid_count(rows: &FoldRows<'_>, validity: &ValidityMask) -> u64 {
    let count = if validity.no_nulls() {
        rows.len()
    } else {
        match rows {
            FoldRows::Span(span) => validity.count_valid_in(span.clone()),
            FoldRows::Picked(picked) => picked
                .iter()
                .filter(|row| validity.is_valid(**row as usize))
                .count(),
        }
    };
    count as u64
}

/// Calls `each` with every valid row of `rows` and its value, in row order.
#[inline]
fn for_valid<T: Copy>(
    rows: &FoldRows<'_>,
    values: &[T],
    validity: &ValidityMask,
    mut each: impl FnMut(usize, T),
) {
    match rows {
        FoldRows::Span(span) => {
            if validity.no_nulls() {
                for (row, value) in span.clone().zip(&values[span.clone()]) {
                    each(row, *value);
                }
            } else if !span.is_empty() {
                // A validity word at a time: a word with no NULL runs the
                // plain loop over its rows, and any other visits its set
                // bits, lowest first, so the rows still arrive in order.
                for index in span.start / 64..=(span.end - 1) / 64 {
                    let mut bits = validity.word_within(index, span);
                    let base = index * 64;
                    if bits == u64::MAX {
                        for (offset, value) in values[base..base + 64].iter().enumerate() {
                            each(base + offset, *value);
                        }
                        continue;
                    }
                    while bits != 0 {
                        let row = base + bits.trailing_zeros() as usize;
                        each(row, values[row]);
                        bits &= bits - 1;
                    }
                }
            }
        }
        FoldRows::Picked(picked) => {
            if validity.no_nulls() {
                for &row in *picked {
                    let row = row as usize;
                    each(row, values[row]);
                }
            } else {
                for &row in *picked {
                    let row = row as usize;
                    if validity.is_valid(row) {
                        each(row, values[row]);
                    }
                }
            }
        }
    }
}

/// The valid rows of `rows`' values, in row order.
fn valid_values<'a, T: Copy>(
    rows: &'a FoldRows<'a>,
    values: &'a [T],
    validity: &'a ValidityMask,
) -> impl Iterator<Item = T> + 'a {
    let (span, picked) = match rows {
        FoldRows::Span(span) => (Some(span.clone()), None),
        FoldRows::Picked(picked) => (None, Some(*picked)),
    };
    span.into_iter()
        .flatten()
        .chain(picked.into_iter().flatten().map(|row| *row as usize))
        .filter(move |row| validity.is_valid(*row))
        .map(move |row| values[row])
}

/// The sum of the valid rows' units and how many there were.
fn unit_total(rows: &FoldRows<'_>, units: &[i64], validity: &ValidityMask) -> (i128, u64) {
    let mut total = 0_i128;
    let mut count = 0_u64;
    // `i64` units cannot carry an `i128` total out of range before 2^64
    // rows, so the add needs no check.
    for_valid(rows, units, validity, |_, value| {
        total = total.wrapping_add(i128::from(value));
        count += 1;
    });
    (total, count)
}

/// The first row, in row order, holding the least (`least`) or greatest
/// value: the row the per-row update would keep, since it replaces only on
/// a strictly better value.
fn extreme_row<T: Copy + Ord>(
    rows: &FoldRows<'_>,
    values: &[T],
    validity: &ValidityMask,
    least: bool,
) -> Option<(usize, T)> {
    let mut best: Option<(usize, T)> = None;
    for_valid(rows, values, validity, |row, value| {
        let better = match best {
            None => true,
            Some((_, current)) => {
                if least {
                    value < current
                } else {
                    value > current
                }
            }
        };
        if better {
            best = Some((row, value));
        }
    });
    best
}

/// Folds one aggregate's column over `rows`, or `false` with nothing
/// applied when the column or the function has no fold here.
#[allow(clippy::too_many_lines)]
fn fold_column(
    batch: &RecordBatch,
    rows: &FoldRows<'_>,
    aggregate: &CompiledAggregate,
    state: &mut AggregateState,
    memory: &MemoryTracker,
) -> Result<bool, ExecError> {
    if aggregate.distinct {
        return fold_distinct(batch, rows, aggregate, state, memory);
    }
    if matches!(
        aggregate.function,
        AggregateFunction::StdDev { .. } | AggregateFunction::Variance { .. }
    ) && let Some(folded) = fold_decimal_moments(batch, rows, aggregate, state)?
    {
        return Ok(folded);
    }
    let Some(expression) = &aggregate.expr else {
        if aggregate.function == AggregateFunction::Count {
            state.add_dense_count(rows.len() as u64)?;
            return Ok(true);
        }
        return Ok(false);
    };
    let Some(column) = expression
        .column_index()
        .and_then(|column| batch.column(column))
    else {
        return Ok(false);
    };
    // Only a plain integer column folds as an integer: a YEAR or a BIT
    // stored as one keeps the per-row update's own reading of it.
    let plain_integer = matches!(
        column.data_type(),
        DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
    );
    let Some((typed, validity)) = column.typed() else {
        return Ok(false);
    };
    match (aggregate.function, typed) {
        (AggregateFunction::Count, _) => {
            state.add_dense_count(valid_count(rows, validity))?;
            Ok(true)
        }
        (
            AggregateFunction::Sum,
            TypedValues::Decimal128 {
                values: DecimalUnits::Narrow(units),
                scale,
                ..
            },
        ) => {
            let (total, count) = unit_total(rows, units, validity);
            if count > 0 {
                state.update_decimal_sum_units(total, *scale, aggregate_uses_float(aggregate))?;
            }
            Ok(true)
        }
        (
            AggregateFunction::Average,
            TypedValues::Decimal128 {
                values: DecimalUnits::Narrow(units),
                scale,
                ..
            },
        ) => {
            let Some(result_scale) = decimal_average_scale(aggregate) else {
                return Ok(false);
            };
            let Some(digits) = result_scale
                .checked_sub(*scale)
                .filter(|digits| *digits <= AVERAGE_MAX_DIGITS)
            else {
                return Ok(false);
            };
            let (total, count) = unit_total(rows, units, validity);
            if count > 0 {
                state.add_decimal_average_partial(total, digits, result_scale, count)?;
            }
            Ok(true)
        }
        // An integer SUM typed as its own column type keeps the per-row
        // update's checked integer total. A batch whose own total leaves
        // the type is handed back, so the per-row update decides it.
        (AggregateFunction::Sum, TypedValues::Int64(values))
            if plain_integer && aggregate.sum_carrier == Some(DataType::Int64) =>
        {
            let mut total = 0_i64;
            let mut count = 0_u64;
            let mut overflow = false;
            for_valid(rows, values, validity, |_, value| {
                match total.checked_add(value) {
                    Some(sum) => total = sum,
                    None => overflow = true,
                }
                count += 1;
            });
            if overflow {
                return Ok(false);
            }
            if count > 0 {
                state.add_dense_signed(total)?;
            }
            Ok(true)
        }
        (AggregateFunction::Sum, TypedValues::UInt64(values))
            if plain_integer && aggregate.sum_carrier == Some(DataType::UInt64) =>
        {
            let mut total = 0_u64;
            let mut count = 0_u64;
            let mut overflow = false;
            for_valid(rows, values, validity, |_, value| {
                match total.checked_add(value) {
                    Some(sum) => total = sum,
                    None => overflow = true,
                }
                count += 1;
            });
            if overflow {
                return Ok(false);
            }
            if count > 0 {
                state.add_dense_unsigned(total)?;
            }
            Ok(true)
        }
        // MIN/MAX over scaled decimal or temporal units: the per-row update
        // compares the same units and formats the winning row's text.
        (
            AggregateFunction::Minimum | AggregateFunction::Maximum,
            TypedValues::Decimal128 {
                values: DecimalUnits::Narrow(units),
                ..
            }
            | TypedValues::Temporal { units, .. },
        ) => {
            let least = aggregate.function == AggregateFunction::Minimum;
            if let Some((row, units)) = extreme_row(rows, units, validity, least) {
                state.update_extreme_units(
                    aggregate,
                    i128::from(units),
                    || typed.format_unit(row),
                    memory,
                )?;
            }
            Ok(true)
        }
        // MIN/MAX over integers: the per-row update retains the integer
        // value with its f64 as the comparison hint.
        (AggregateFunction::Minimum | AggregateFunction::Maximum, TypedValues::Int64(values))
            if plain_integer =>
        {
            let least = aggregate.function == AggregateFunction::Minimum;
            if let Some((_, value)) = extreme_row(rows, values, validity, least) {
                #[allow(clippy::cast_precision_loss)]
                let number = value as f64;
                state.update_with_number(aggregate, &Value::Int64(value), Some(number), memory)?;
            }
            Ok(true)
        }
        (AggregateFunction::Minimum | AggregateFunction::Maximum, TypedValues::UInt64(values))
            if plain_integer =>
        {
            let least = aggregate.function == AggregateFunction::Minimum;
            if let Some((_, value)) = extreme_row(rows, values, validity, least) {
                #[allow(clippy::cast_precision_loss)]
                let number = value as f64;
                state.update_with_number(aggregate, &Value::UInt64(value), Some(number), memory)?;
            }
            Ok(true)
        }
        // A double SUM or AVG adds each row's double in row order, and
        // STDDEV/VARIANCE take Welford's step per row over the double the
        // per-row update reads from an integer or a double: folded here in
        // the same order with the same operations.
        (AggregateFunction::Sum | AggregateFunction::Average, TypedValues::Float64(values))
            if aggregate_uses_float(aggregate) =>
        {
            state.fold_observations(valid_values(rows, values, validity))
        }
        (AggregateFunction::StdDev { .. } | AggregateFunction::Variance { .. }, _) =>
        {
            #[allow(clippy::cast_precision_loss)]
            match typed {
                TypedValues::Float64(values) => {
                    state.fold_observations(valid_values(rows, values, validity))
                }
                TypedValues::Int64(values) if plain_integer => state.fold_observations(
                    valid_values(rows, values, validity).map(|value| value as f64),
                ),
                TypedValues::UInt64(values) if plain_integer => state.fold_observations(
                    valid_values(rows, values, validity).map(|value| value as f64),
                ),
                _ => Ok(false),
            }
        }
        // BIT_AND/OR/XOR read a row as BIGINT UNSIGNED: a negative signed
        // integer by its two's-complement bits.
        (
            AggregateFunction::BitAnd | AggregateFunction::BitOr | AggregateFunction::BitXor,
            TypedValues::Int64(values),
        ) if plain_integer => Ok(state.fold_bits(
            aggregate.function,
            valid_values(rows, values, validity)
                .map(|value| u64::from_ne_bytes(value.to_ne_bytes())),
        )),
        (
            AggregateFunction::BitAnd | AggregateFunction::BitOr | AggregateFunction::BitXor,
            TypedValues::UInt64(values),
        ) if plain_integer => {
            Ok(state.fold_bits(aggregate.function, valid_values(rows, values, validity)))
        }
        // MIN/MAX over text, under the aggregate's collation: the per-row
        // update keeps the first row whose value no later row beats, which
        // is the winner here too, and it compares that one value with the
        // state's under the same collation.
        (AggregateFunction::Minimum | AggregateFunction::Maximum, TypedValues::Utf8(text))
            if text_extreme_applies(column.data_type(), aggregate) =>
        {
            let least = aggregate.function == AggregateFunction::Minimum;
            let winner = text_extreme_row(rows, text, validity, aggregate.collation, least);
            if matches!(winner, TextExtreme::Declined) {
                return Ok(false);
            }
            if let TextExtreme::Row(row) = winner {
                let value = column
                    .value_owned(row)
                    .ok_or(ExecError::InvalidBatch("aggregate row outside its batch"))?;
                state.update_with_number(aggregate, &value, None, memory)?;
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Whether a text MIN/MAX compares the way `text_extreme_row` does: plain
/// text under a text collation. JSON, decimals and TIME carried as text
/// have their own orders in the per-row comparison.
fn text_extreme_applies(column_type: DataType, aggregate: &CompiledAggregate) -> bool {
    column_type == DataType::Utf8
        && aggregate.data_type == Some(DataType::Utf8)
        && aggregate.collation != Collation::Json
}

/// The first row, per dictionary code, of the valid rows of `rows`, or
/// `None` when a code is outside the dictionary or the dictionary is too
/// large next to the rows for a per-code table to pay.
fn first_row_per_code(
    rows: &FoldRows<'_>,
    codes: &[u32],
    entries: usize,
    validity: &ValidityMask,
) -> Option<Vec<u32>> {
    if entries > rows.len().saturating_mul(4).saturating_add(1_024) {
        return None;
    }
    let mut first = vec![u32::MAX; entries];
    let mut outside = false;
    for_valid(rows, codes, validity, |row, code| {
        match first.get_mut(code as usize) {
            Some(slot) if *slot == u32::MAX => {
                *slot = u32::try_from(row).unwrap_or(u32::MAX - 1);
            }
            Some(_) => {}
            None => outside = true,
        }
    });
    (!outside).then_some(first)
}

/// What a text MIN/MAX fold found over one batch's rows.
enum TextExtreme {
    /// Text that is not UTF-8: the per-row update reads it its own way.
    Declined,
    /// Every row was NULL.
    AllNull,
    /// The row holding the extreme.
    Row(usize),
}

/// The row the per-row MIN (`least`) or MAX update would keep over `rows`
/// of a text column: the first row, in row order, of the values no other
/// value beats under `collation`.
///
/// A coded column compares each distinct entry present once rather than
/// every row: among entries that tie under the collation (`'a'` and `'A'`
/// under a case-insensitive one) the winner is the one whose first row
/// comes first, exactly the row the per-row loop would have kept.
fn text_extreme_row(
    rows: &FoldRows<'_>,
    column: &StrColumn,
    validity: &ValidityMask,
    collation: Collation,
    least: bool,
) -> TextExtreme {
    let wins = |ordering: std::cmp::Ordering| {
        if least {
            ordering.is_lt()
        } else {
            ordering.is_gt()
        }
    };
    if let Some((codes, entries)) = column.dictionary()
        && let Some(first) = first_row_per_code(rows, codes, entries.len(), validity)
    {
        let mut best: Option<(usize, u32)> = None;
        for (code, row) in first.iter().copied().enumerate() {
            if row == u32::MAX {
                continue;
            }
            best = match best {
                None => Some((code, row)),
                Some((best_code, best_row)) => {
                    let ordering = super::compare_collated_text(
                        &entries[code],
                        &entries[best_code],
                        collation,
                    );
                    if wins(ordering) || (ordering.is_eq() && row < best_row) {
                        Some((code, row))
                    } else {
                        Some((best_code, best_row))
                    }
                }
            };
        }
        return best.map_or(TextExtreme::AllNull, |(_, row)| {
            TextExtreme::Row(row as usize)
        });
    }
    let views = column.views();
    let heap = column.heap();
    let ascii_ranks = printable_ascii_ranks(collation);
    let mut best: Option<(usize, String)> = None;
    let mut invalid = false;
    let mut visit = |row: usize| {
        views[row].with_bytes(heap, |bytes| {
            let Ok(text) = std::str::from_utf8(bytes) else {
                invalid = true;
                return;
            };
            let better = match &best {
                None => true,
                Some((_, current)) => wins(
                    ascii_ranks
                        .and_then(|ranks| ranked_ascii_order(text, current, ranks))
                        .unwrap_or_else(|| super::compare_collated_text(text, current, collation)),
                ),
            };
            if better {
                best = Some((row, text.to_owned()));
            }
        });
    };
    match rows {
        FoldRows::Span(span) => span
            .clone()
            .filter(|row| validity.is_valid(*row))
            .for_each(&mut visit),
        FoldRows::Picked(picked) => picked
            .iter()
            .map(|row| *row as usize)
            .filter(|row| validity.is_valid(*row))
            .for_each(&mut visit),
    }
    if invalid {
        return TextExtreme::Declined;
    }
    best.map_or(TextExtreme::AllNull, |(row, _)| TextExtreme::Row(row))
}

/// Each printable ASCII character's place in `collation`'s order, for a
/// collation where two printable ASCII strings compare as the sequences of
/// those places, the shorter first on a common prefix: `utf8mb4_0900_ai_ci`,
/// which weighs every printable ASCII character with one primary weight
/// (none is ignorable and none starts a contraction in the root order) and
/// pads nothing. Built once from the collation's own comparison of the
/// single characters; `None` for any other collation, or if a character
/// turns out ignorable.
pub(super) fn printable_ascii_ranks(collation: Collation) -> Option<&'static [u8; 128]> {
    static AI_CI: std::sync::OnceLock<Option<[u8; 128]>> = std::sync::OnceLock::new();
    if collation != Collation::Utf8mb40900AiCi {
        return None;
    }
    AI_CI
        .get_or_init(|| {
            let text = |byte: u8| char::from(byte).to_string();
            let mut printable: Vec<u8> = (0x20..0x7f).collect();
            if printable
                .iter()
                .any(|byte| super::compare_collated_text(&text(*byte), "", collation).is_eq())
            {
                return None;
            }
            printable.sort_by(|left, right| {
                super::compare_collated_text(&text(*left), &text(*right), collation)
            });
            let mut ranks = [0_u8; 128];
            let mut rank = 0_u8;
            for pair in 0..printable.len() {
                if pair > 0
                    && super::compare_collated_text(
                        &text(printable[pair - 1]),
                        &text(printable[pair]),
                        collation,
                    )
                    .is_ne()
                {
                    rank += 1;
                }
                ranks[usize::from(printable[pair])] = rank;
            }
            Some(ranks)
        })
        .as_ref()
}

/// `left` against `right` by `ranks` when both are printable ASCII;
/// `None` otherwise, for the collation's own comparison to decide.
pub(super) fn ranked_ascii_order(
    left: &str,
    right: &str,
    ranks: &[u8; 128],
) -> Option<std::cmp::Ordering> {
    let printable = |text: &str| text.bytes().all(|byte| (0x20..0x7f).contains(&byte));
    if !printable(left) || !printable(right) {
        return None;
    }
    for (left, right) in left.bytes().zip(right.bytes()) {
        let ordering = ranks[usize::from(left)].cmp(&ranks[usize::from(right)]);
        if ordering.is_ne() {
            return Some(ordering);
        }
    }
    Some(left.len().cmp(&right.len()))
}

/// Largest magnitude of decimal units a double holds exactly.
const EXACT_DOUBLE_UNITS: u64 = 1 << 53;
/// Largest scale whose power of ten a double holds exactly.
const EXACT_DOUBLE_SCALE: u8 = 22;

/// `STDDEV`/`VARIANCE` over a DECIMAL column. The binder hands the
/// aggregate the column read as a double, which the per-row update gets by
/// formatting the row's units and parsing the text: the double nearest the
/// decimal. Units within 2^53 over a power of ten within 10^22 are two
/// exact doubles, and their quotient rounds once to that same nearest
/// double, so the fold divides instead. `None` when the argument is not
/// that shape; `Some(false)`, with nothing applied, for a batch holding a
/// value outside the exact range or text the units do not derive.
fn fold_decimal_moments(
    batch: &RecordBatch,
    rows: &FoldRows<'_>,
    aggregate: &CompiledAggregate,
    state: &mut AggregateState,
) -> Result<Option<bool>, ExecError> {
    let Some((index, scale)) = aggregate
        .expr
        .as_ref()
        .and_then(super::CompiledExpr::decimal_column_as_double)
    else {
        return Ok(None);
    };
    let Some((typed, validity)) = batch.column(index).and_then(super::ColumnVector::typed) else {
        return Ok(Some(false));
    };
    let TypedValues::Decimal128 {
        values: DecimalUnits::Narrow(units),
        ..
    } = typed
    else {
        return Ok(Some(false));
    };
    if typed.unit_kind() != Some(UnitKind::Decimal { scale }) || scale > EXACT_DOUBLE_SCALE {
        return Ok(Some(false));
    }
    let mut widest = 0_u64;
    for_valid(rows, units, validity, |_, value| {
        widest = widest.max(value.unsigned_abs());
    });
    if widest > EXACT_DOUBLE_UNITS {
        return Ok(Some(false));
    }
    let divisor = 10_f64.powi(i32::from(scale));
    #[allow(clippy::cast_precision_loss)] // checked exact above
    state
        .fold_observations(valid_values(rows, units, validity).map(|value| value as f64 / divisor))
        .map(Some)
}

/// `COUNT(DISTINCT column)` by column: a DECIMAL, DATE or DATETIME column
/// by its packed units, a dictionary-coded text column by the entries
/// present, and any other text column by each row's bytes with no value
/// built. `false`, with nothing applied, for every other distinct shape.
fn fold_distinct(
    batch: &RecordBatch,
    rows: &FoldRows<'_>,
    aggregate: &CompiledAggregate,
    state: &mut AggregateState,
    memory: &MemoryTracker,
) -> Result<bool, ExecError> {
    if aggregate.function != AggregateFunction::Count {
        return Ok(false);
    }
    let Some(column) = aggregate
        .expr
        .as_ref()
        .and_then(super::CompiledExpr::column_index)
        .and_then(|column| batch.column(column))
    else {
        return Ok(false);
    };
    let Some((typed, validity)) = column.typed() else {
        return Ok(false);
    };
    // Units are the key only while the state keys by the same kind: a
    // batch packed any other way takes the per-row update, which reads its
    // text back to the state's units.
    if let Some(kind) = state.distinct_units() {
        if typed.unit_kind() != Some(kind) {
            return Ok(false);
        }
        let (TypedValues::Decimal128 {
            values: DecimalUnits::Narrow(units),
            ..
        }
        | TypedValues::Temporal { units, .. }) = typed
        else {
            return Ok(false);
        };
        let step = kind.step();
        let mut previous = None;
        let mut keys = Vec::with_capacity(rows.len());
        for_valid(rows, units, validity, |_, value| {
            if previous == Some(value) {
                return;
            }
            previous = Some(value);
            keys.push(i128::from(if step == 1 {
                value
            } else {
                value.div_euclid(step)
            }));
        });
        state.update_distinct_count_ints(&mut keys, &mut Vec::new(), memory)?;
        return Ok(true);
    }
    if column.data_type() != DataType::Utf8 {
        return Ok(false);
    }
    let TypedValues::Utf8(text) = typed else {
        return Ok(false);
    };
    fold_distinct_text(column, text, validity, rows, aggregate, state, memory)
}

/// The text half of [`fold_distinct`]: a dictionary-coded column inserts
/// each entry present once, and any other column stages every valid row's
/// key and inserts the batch together. `false`, with nothing applied, when
/// the dictionary is too large for a per-code table or the state's set no
/// longer keys text by weight.
fn fold_distinct_text(
    column: &crate::ColumnVector,
    text: &StrColumn,
    validity: &ValidityMask,
    rows: &FoldRows<'_>,
    aggregate: &CompiledAggregate,
    state: &mut AggregateState,
    memory: &MemoryTracker,
) -> Result<bool, ExecError> {
    if let Some((codes, entries)) = text.dictionary() {
        let Some(first) = first_row_per_code(rows, codes, entries.len(), validity) else {
            return Ok(false);
        };
        for (code, _) in first
            .into_iter()
            .enumerate()
            .filter(|(_, row)| *row != u32::MAX)
        {
            state.update_distinct_count_text(aggregate, &entries[code], memory)?;
        }
        return Ok(true);
    }
    let Some(mut stager) = state.stage_distinct_texts() else {
        return Ok(false);
    };
    let views = text.views();
    let heap = text.heap();
    // Rows that are not UTF-8 wait for the per-row update, which reads
    // them its own way.
    let mut unreadable = Vec::new();
    let mut visit = |row: usize| {
        views[row].with_bytes(heap, |bytes| {
            if let Ok(text) = std::str::from_utf8(bytes) {
                stager.stage(text, memory)
            } else {
                unreadable.push(row);
                Ok(())
            }
        })
    };
    match rows {
        FoldRows::Span(span) => span
            .clone()
            .filter(|row| validity.is_valid(*row))
            .try_for_each(&mut visit)?,
        FoldRows::Picked(picked) => picked
            .iter()
            .map(|row| *row as usize)
            .filter(|row| validity.is_valid(*row))
            .try_for_each(&mut visit)?,
    }
    stager.finish(memory)?;
    for row in unreadable {
        let value = column
            .value_owned(row)
            .ok_or(ExecError::InvalidBatch("aggregate row outside its batch"))?;
        state.update_with_number(aggregate, &value, None, memory)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{Collation, printable_ascii_ranks, ranked_ascii_order};

    /// The rank table orders printable ASCII exactly as the collation does,
    /// over strings drawn to collide: case pairs, punctuation, digits,
    /// spaces inside and at the end, and shared prefixes.
    #[test]
    fn ascii_ranks_agree_with_the_collation() {
        let collation = Collation::Utf8mb40900AiCi;
        let ranks = printable_ascii_ranks(collation).expect("ranks");
        let mut seed = 0x1234_5678_u64;
        let mut next = || {
            seed = crate::batch::mix64(seed);
            seed
        };
        let alphabet: Vec<u8> = (0x20..0x7f).collect();
        let draw = |next: &mut dyn FnMut() -> u64| {
            let length = usize::try_from(next() % 7).expect("small");
            (0..length)
                .map(|_| {
                    let pick = next();
                    // Half the characters from a narrow set, so strings tie
                    // and share prefixes often.
                    if pick.is_multiple_of(2) {
                        char::from(b"aAbB -_.0"[usize::try_from(pick / 2 % 9).expect("small")])
                    } else {
                        char::from(
                            alphabet
                                [usize::try_from(pick / 2 % alphabet.len() as u64).expect("small")],
                        )
                    }
                })
                .collect::<String>()
        };
        for _ in 0..200_000 {
            let left = draw(&mut next);
            let right = draw(&mut next);
            assert_eq!(
                ranked_ascii_order(&left, &right, ranks),
                Some(super::super::compare_collated_text(
                    &left, &right, collation
                )),
                "{left:?} vs {right:?}"
            );
        }
        assert_eq!(ranked_ascii_order("é", "e", ranks), None);
        assert!(printable_ascii_ranks(Collation::Utf8mb4Bin).is_none());
    }
}
