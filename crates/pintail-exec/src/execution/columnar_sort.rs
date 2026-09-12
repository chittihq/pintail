//! An in-memory sort over the input's own batches.
//!
//! The row sort clones every cell into a row, orders the rows, and clones
//! the cells back into columns. This one keeps the input's batches as they
//! arrived, orders references to their rows, and gathers each output
//! column once, from packed buffers where the columns have them.
//!
//! The order is the row sort's exactly. Each key compares as
//! [`compare_sort_values`] compares the values the row sort would hold: a
//! key whose packed form orders as those values do - integers, decimals
//! whose text is derived from their units, temporals within the years their
//! canonical text spells in order, plain text under the key's collation -
//! compares in that form; any other compares the values themselves. The
//! sort is stable, so rows with equal keys keep the order they arrived in,
//! as the row sort keeps them.

use std::cmp::Ordering;

use pintail_sql::BoundOrderKey;
use pintail_types::{DataType, Value};

use super::gather::plain_text;
use super::join::collation_sort_key;
use super::sort::compare_sort_values;
use super::{ExecError, MemoryTracker};
use crate::batch::{ColumnVector, TypedValues};
use crate::collation::Collation;
use crate::expression::compare_utf8_mysql;
use crate::{DEFAULT_BATCH_ROWS, RecordBatch};

/// Bytes a kept row reference and its position in the order cost.
const REFERENCE_BYTES: usize = size_of::<(u32, u32)>() + size_of::<u32>();

/// Bytes one row costs a packed sort key.
const KEY_BYTES: usize = size_of::<i128>() + size_of::<bool>();

/// What a prepared text key may average per row before the sort keeps the
/// comparator instead. Weight keys run a few bytes per character, and a
/// column of long text would otherwise hold a second copy of itself.
const PREPARED_KEY_BYTES_PER_ROW: usize = 64;

/// What keeping `batch` for a sort by `keys` holds: the batch, and per
/// visible row its reference and packed keys.
pub(super) fn retained_bytes(batch: &RecordBatch, keys: usize) -> usize {
    batch.estimated_bytes().saturating_add(
        batch
            .visible_row_count()
            .saturating_mul(REFERENCE_BYTES.saturating_add(keys.saturating_mul(KEY_BYTES))),
    )
}

/// One key, in the form its rows compare in.
enum SortKey {
    /// Values that order as these integers: `None` is NULL.
    Units(Vec<Option<i128>>),
    /// Plain text of every kept batch, compared under `collation`.
    Text { column: usize, collation: Collation },
    /// One collation weight key per kept row, `None` for NULL. Comparing
    /// these bytewise is comparing the text under the collation, so the
    /// collation runs once per row instead of inside every comparison.
    Prepared(Vec<Option<Vec<u8>>>),
    /// The values themselves.
    Values { column: usize, collation: Collation },
}

/// Rows sorted by reference, gathered into output batches on demand.
pub(super) struct ColumnarSorted {
    batches: Vec<RecordBatch>,
    /// Each kept row as (batch, row), in sorted order.
    order: Vec<(u32, u32)>,
    position: usize,
    /// The output's width: the input's, less the sort-only columns.
    width: Option<usize>,
}

impl ColumnarSorted {
    /// The kept rows in order, the first `limit` of them when one is given.
    pub(super) fn new(
        batches: Vec<RecordBatch>,
        keys: &[BoundOrderKey],
        width: Option<usize>,
        collation: Collation,
        limit: Option<usize>,
    ) -> Result<Self, ExecError> {
        let rows = batches
            .iter()
            .enumerate()
            .flat_map(|(index, batch)| {
                let index = u32::try_from(index).unwrap_or(u32::MAX);
                batch
                    .selection()
                    .selected_rows()
                    .map(move |row| (index, u32::try_from(row).unwrap_or(u32::MAX)))
            })
            .collect::<Vec<_>>();
        let sort_keys = keys
            .iter()
            .map(|key| sort_key(&batches, &rows, *key, collation))
            .collect::<Result<Vec<_>, _>>()?;
        // Rows with equal keys order by arrival, as a stable sort keeps them,
        // so the order is total and the first `limit` of it well defined.
        let compare = |left: &usize, right: &usize| {
            keys.iter()
                .zip(&sort_keys)
                .map(|(key, sort_key)| compare_key(&batches, &rows, sort_key, *key, *left, *right))
                .find(|ordering| ordering.is_ne())
                .unwrap_or_else(|| left.cmp(right))
        };
        let mut positions = (0..rows.len()).collect::<Vec<_>>();
        if let Some(limit) = limit.filter(|limit| *limit < positions.len()) {
            if limit > 0 {
                positions.select_nth_unstable_by(limit - 1, compare);
            }
            positions.truncate(limit);
        }
        positions.sort_unstable_by(compare);
        crate::counters::count(|counters| {
            counters.rows_sorted = counters
                .rows_sorted
                .saturating_add(u64::try_from(rows.len()).unwrap_or(u64::MAX));
        });
        Ok(Self {
            order: positions
                .into_iter()
                .map(|position| rows[position])
                .collect(),
            batches,
            position: 0,
            width,
        })
    }

    /// Each kept row as (batch, row), in sorted order.
    pub(super) fn into_order(self) -> Vec<(u32, u32)> {
        self.order
    }

    fn width(&self) -> usize {
        let columns = self
            .batches
            .first()
            .map_or(0, |batch| batch.columns().len());
        self.width.map_or(columns, |width| width.min(columns))
    }

    /// The next sorted row as values, for consumers that merge rows.
    pub(super) fn next_row(&mut self) -> Option<Vec<Value>> {
        let (batch, row) = *self.order.get(self.position)?;
        self.position += 1;
        let batch = &self.batches[batch as usize];
        Some(
            (0..self.width())
                .map(|column| {
                    batch
                        .column(column)
                        .and_then(|column| column.value_owned(row as usize))
                        .unwrap_or(Value::Null)
                })
                .collect(),
        )
    }

    pub(super) fn next_batch(
        &mut self,
        column_types: &[DataType],
        memory: &MemoryTracker,
    ) -> Result<Option<RecordBatch>, ExecError> {
        if self.position >= self.order.len() {
            return Ok(None);
        }
        let end = self
            .position
            .saturating_add(DEFAULT_BATCH_ROWS)
            .min(self.order.len());
        let picks = &self.order[self.position..end];
        let columns = column_types
            .iter()
            .take(self.width())
            .enumerate()
            .map(|(column, data_type)| gather(&self.batches, picks, column, *data_type))
            .collect::<Result<Vec<_>, _>>()?;
        let batch = RecordBatch::new(picks.len(), columns)?;
        memory.ensure_transient(batch.estimated_bytes())?;
        self.position = end;
        Ok(Some(batch))
    }
}

/// Kept batches a top-k sort holds before it cuts them down to k rows.
const MAX_KEPT_BATCHES: usize = 32;

/// A top-k sort cuts its kept batches once they hold this fraction of the
/// query's ceiling, so the input it is still reading has room.
const KEPT_SHARE: usize = 4;

/// What a top-k sort kept: its rows in order, or the batches it held when
/// they stopped fitting as columns, for the row sort to continue with.
pub(super) enum TopK {
    Sorted(ColumnarSorted),
    Unkept(Vec<RecordBatch>),
}

/// The first `k` rows of the input in the row sort's order, with rows of
/// equal keys in arrival order. Batches are kept whole until they hold
/// twice `k` rows, `MAX_KEPT_BATCHES` past the last cut or a `KEPT_SHARE`
/// of the ceiling, then cut to their first `k` rows, gathered into batches
/// of their own. The k-th row's first key
/// is then a cutoff: later rows whose first key orders after it cannot
/// enter the first `k` and are left out before they are kept.
pub(super) fn top_k(
    input: &mut super::PullOperator,
    k: usize,
    keys: &[BoundOrderKey],
    width: Option<usize>,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<TopK, ExecError> {
    if k == 0 {
        return ColumnarSorted::new(Vec::new(), keys, width, collation, Some(0)).map(TopK::Sorted);
    }
    let cut_at = k.saturating_mul(2).max(DEFAULT_BATCH_ROWS);
    let mut kept = Vec::new();
    // Batches holding the first k rows of the last cut.
    let mut cut = 0_usize;
    let mut rows = 0_usize;
    let mut reserved = 0_usize;
    let mut cutoff = None;
    while let Some(mut batch) = input.next_batch(memory)? {
        if let (Some(cutoff), Some(key)) = (&cutoff, keys.first()) {
            narrow(&mut batch, cutoff, *key)?;
        }
        let visible = batch.visible_row_count();
        if visible == 0 {
            continue;
        }
        let bytes = retained_bytes(&batch, keys.len());
        if memory.reserve(bytes).is_err() {
            memory.release(reserved);
            kept.push(batch);
            return Ok(TopK::Unkept(kept));
        }
        reserved = reserved.saturating_add(bytes);
        rows = rows.saturating_add(visible);
        kept.push(batch);
        if rows < cut_at
            && kept.len() <= cut + MAX_KEPT_BATCHES
            && reserved <= memory.limit() / KEPT_SHARE
        {
            continue;
        }
        let Some(types) = uniform_types(&kept) else {
            memory.release(reserved);
            return Ok(TopK::Unkept(kept));
        };
        let mut first = ColumnarSorted::new(kept, keys, None, collation, Some(k))?;
        kept = Vec::new();
        while let Some(batch) = first.next_batch(&types, memory)? {
            kept.push(batch);
        }
        cut = kept.len();
        rows = kept.iter().map(RecordBatch::visible_row_count).sum();
        let now = kept
            .iter()
            .map(|batch| retained_bytes(batch, keys.len()))
            .sum::<usize>();
        memory.release(reserved);
        // The first k rows alone crowd the ceiling: the row top-k, which
        // holds only them, continues from here.
        if now > memory.limit() / KEPT_SHARE || memory.reserve(now).is_err() {
            return Ok(TopK::Unkept(kept));
        }
        reserved = now;
        cutoff = (rows == k)
            .then(|| keys.first().and_then(|key| Cutoff::of(&kept, *key)))
            .flatten();
    }
    ColumnarSorted::new(kept, keys, width, collation, Some(k)).map(TopK::Sorted)
}

/// Each column's type, when every batch gives it the same one.
fn uniform_types(batches: &[RecordBatch]) -> Option<Vec<DataType>> {
    let types = batches
        .first()?
        .columns()
        .iter()
        .map(ColumnVector::data_type)
        .collect::<Vec<_>>();
    batches
        .iter()
        .all(|batch| {
            batch.columns().len() == types.len()
                && batch
                    .columns()
                    .iter()
                    .zip(&types)
                    .all(|(column, data_type)| column.data_type() == *data_type)
        })
        .then_some(types)
}

/// The first sort key of a top-k sort's k-th row, as ordering units.
struct Cutoff {
    data_type: DataType,
    units: Option<i128>,
}

impl Cutoff {
    /// The last kept row's first key, when its column orders as units.
    fn of(kept: &[RecordBatch], key: BoundOrderKey) -> Option<Self> {
        let batch = kept.last()?;
        let row = batch.selection().selected_rows().last()?;
        let column = batch.column(key.index)?;
        let years = four_digit_years(column.data_type());
        let units = units_at(column, row, key, years.as_ref()).ok()?;
        Some(Self {
            data_type: column.data_type(),
            units,
        })
    }
}

/// Leaves out of `batch` the rows whose first key orders after the cutoff.
/// A column that does not order as units leaves the batch whole.
fn narrow(batch: &mut RecordBatch, cutoff: &Cutoff, key: BoundOrderKey) -> Result<(), ExecError> {
    let Some(column) = batch.column(key.index) else {
        return Ok(());
    };
    if column.data_type() != cutoff.data_type {
        return Ok(());
    }
    let years = four_digit_years(cutoff.data_type);
    let mut selection = batch.selection().clone();
    for row in batch.selection().selected_rows() {
        let Ok(units) = units_at(column, row, key, years.as_ref()) else {
            return Ok(());
        };
        let ordering = null_order(key, units.is_none(), cutoff.units.is_none())
            .unwrap_or_else(|| directed(units.cmp(&cutoff.units), key));
        if ordering.is_gt() {
            selection.set(row, false)?;
        }
    }
    batch.set_selection(selection)?;
    Ok(())
}

/// The years `0000` to `9999` in `data_type`'s units, where canonical text
/// has a fixed width and so orders as time does.
fn four_digit_years(data_type: DataType) -> Option<std::ops::RangeInclusive<i128>> {
    const MICROS_PER_DAY: i128 = 86_400_000_000;
    let day = |year, month, day| {
        chrono::NaiveDate::from_ymd_opt(year, month, day).map(|date| {
            i128::from(
                date.signed_duration_since(chrono::NaiveDate::default())
                    .num_days(),
            )
        })
    };
    let (first, last) = (day(0, 1, 1)?, day(9999, 12, 31)?);
    match data_type {
        DataType::Date32 => Some(first..=last),
        DataType::DateTime64 { .. } => {
            Some(first * MICROS_PER_DAY..=(last + 1) * MICROS_PER_DAY - 1)
        }
        _ => None,
    }
}

/// A column whose values do not order as its packed units do.
struct Unordered;

/// Row `row` of a packed column as ordering units, `None` for NULL.
fn units_at(
    column: &ColumnVector,
    row: usize,
    key: BoundOrderKey,
    years: Option<&std::ops::RangeInclusive<i128>>,
) -> Result<Option<i128>, Unordered> {
    let (typed, validity) = column.typed().ok_or(Unordered)?;
    if !validity.is_valid(row) {
        return Ok(None);
    }
    let units = match (column.data_type(), typed) {
        (data_type, TypedValues::Int64(values)) if data_type.storage_type() == DataType::Int64 => {
            i128::from(values[row])
        }
        (data_type, TypedValues::UInt64(values))
            if data_type.storage_type() == DataType::UInt64 =>
        {
            i128::from(values[row])
        }
        // A decimal key compares by value; its derived text is canonical.
        (DataType::Decimal { .. }, TypedValues::Decimal128 { values, text, .. })
            if key.decimal && text.derived() =>
        {
            values.get(row).ok_or(Unordered)?
        }
        // Temporal text is compared as text; derived text is canonical, and
        // within four-digit years it orders as the units do.
        (
            data_type @ (DataType::Date32 | DataType::DateTime64 { .. }),
            TypedValues::Temporal { units, text },
        ) if !key.decimal && text.derived() => {
            let unit = units[row];
            let spelled = match data_type {
                DataType::DateTime64 { fsp } => {
                    let step = 10_i64.pow(6 - u32::from(fsp.min(6)));
                    unit - unit.rem_euclid(step)
                }
                _ => unit,
            };
            let spelled = i128::from(spelled);
            if !years.is_some_and(|years| years.contains(&spelled)) {
                return Err(Unordered);
            }
            spelled
        }
        _ => return Err(Unordered),
    };
    Ok(Some(units))
}

fn sort_key(
    batches: &[RecordBatch],
    rows: &[(u32, u32)],
    key: BoundOrderKey,
    collation: Collation,
) -> Result<SortKey, ExecError> {
    let collation = key
        .collation
        .and_then(Collation::from_mysql_name)
        .unwrap_or(collation);
    let column = key.index;
    let columns = batches
        .iter()
        .map(|batch| {
            batch.column(column).ok_or(ExecError::InvalidBatch(
                "sort key is outside an input column",
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let data_type = columns.first().map(|column| column.data_type());
    let homogeneous = columns
        .iter()
        .all(|column| Some(column.data_type()) == data_type);
    if homogeneous {
        let years = data_type.and_then(four_digit_years);
        let units = rows
            .iter()
            .map(|&(batch, row)| {
                units_at(columns[batch as usize], row as usize, key, years.as_ref())
            })
            .collect::<Result<Vec<_>, Unordered>>();
        if let Ok(units) = units {
            return Ok(SortKey::Units(units));
        }
        // Text the row sort would read lossily compares as its values.
        let readable = columns.iter().all(|column| {
            plain_text(column).is_some_and(|(text, validity)| {
                (0..text.len()).all(|row| {
                    !validity.is_valid(row)
                        || text.views()[row]
                            .with_bytes(text.heap(), |bytes| std::str::from_utf8(bytes).is_ok())
                })
            })
        });
        if readable && !key.decimal {
            // Weight keys are built once per row, which turns the
            // collation work from O(n log n) comparisons into O(n). They
            // are also variable width and held for the whole sort, so a
            // set that would outgrow the rows themselves stays on the
            // comparator.
            let budget = rows.len().saturating_mul(PREPARED_KEY_BYTES_PER_ROW);
            let mut held = 0_usize;
            let mut prepared = Vec::with_capacity(rows.len());
            for &(batch, row) in rows {
                let (text, validity) =
                    plain_text(columns[batch as usize]).expect("readable text key");
                let row = row as usize;
                if !validity.is_valid(row) {
                    prepared.push(None);
                    continue;
                }
                let weights = text.views()[row].with_bytes(text.heap(), |bytes| {
                    collation_sort_key(std::str::from_utf8(bytes).unwrap_or_default(), collation)
                });
                held = held.saturating_add(weights.len());
                if held > budget {
                    return Ok(SortKey::Text { column, collation });
                }
                prepared.push(Some(weights));
            }
            return Ok(SortKey::Prepared(prepared));
        }
    }
    Ok(SortKey::Values { column, collation })
}

/// NULL's place, which the row sort fixes whatever the direction.
const fn null_order(key: BoundOrderKey, left_null: bool, right_null: bool) -> Option<Ordering> {
    match (left_null, right_null) {
        (true, true) => Some(Ordering::Equal),
        (true, false) if key.nulls_first => Some(Ordering::Less),
        (true, false) => Some(Ordering::Greater),
        (false, true) if key.nulls_first => Some(Ordering::Greater),
        (false, true) => Some(Ordering::Less),
        (false, false) => None,
    }
}

fn directed(ordering: Ordering, key: BoundOrderKey) -> Ordering {
    if key.ascending {
        ordering
    } else {
        ordering.reverse()
    }
}

fn compare_key(
    batches: &[RecordBatch],
    rows: &[(u32, u32)],
    sort_key: &SortKey,
    key: BoundOrderKey,
    left: usize,
    right: usize,
) -> Ordering {
    let cell = |position: usize, column: usize| {
        let (batch, row) = rows[position];
        (
            batches[batch as usize]
                .column(column)
                .expect("sort key column"),
            row as usize,
        )
    };
    match sort_key {
        SortKey::Units(units) => {
            let (left, right) = (units[left], units[right]);
            null_order(key, left.is_none(), right.is_none())
                .unwrap_or_else(|| directed(left.cmp(&right), key))
        }
        SortKey::Prepared(keys) => {
            let (left, right) = (&keys[left], &keys[right]);
            null_order(key, left.is_none(), right.is_none())
                .unwrap_or_else(|| directed(left.cmp(right), key))
        }
        SortKey::Text { column, collation } => {
            let ((left, left_row), (right, right_row)) =
                (cell(left, *column), cell(right, *column));
            let ((left, left_valid), (right, right_valid)) = (
                plain_text(left).expect("plain text key"),
                plain_text(right).expect("plain text key"),
            );
            let (left_null, right_null) = (
                !left_valid.is_valid(left_row),
                !right_valid.is_valid(right_row),
            );
            null_order(key, left_null, right_null).unwrap_or_else(|| {
                left.views()[left_row].with_bytes(left.heap(), |left| {
                    right.views()[right_row].with_bytes(right.heap(), |right| {
                        let left = std::str::from_utf8(left).unwrap_or_default();
                        let right = std::str::from_utf8(right).unwrap_or_default();
                        directed(compare_utf8_mysql(left, right, *collation), key)
                    })
                })
            })
        }
        SortKey::Values { column, collation } => {
            let ((left, left_row), (right, right_row)) =
                (cell(left, *column), cell(right, *column));
            compare_sort_values(
                left.value(left_row).unwrap_or(&Value::Null),
                right.value(right_row).unwrap_or(&Value::Null),
                key,
                *collation,
            )
        }
    }
}

/// Output column `column` for `picks`, from the kept batches.
fn gather(
    batches: &[RecordBatch],
    picks: &[(u32, u32)],
    column: usize,
    data_type: DataType,
) -> Result<ColumnVector, ExecError> {
    let sources = batches
        .iter()
        .map(|batch| {
            batch.column(column).ok_or(ExecError::InvalidBatch(
                "sort output is outside an input column",
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    super::gather::gather(&sources, picks, data_type)
}

#[cfg(test)]
mod tests {
    use pintail_sql::BoundOrderKey;
    use pintail_types::{DataType, Value};

    use super::ColumnarSorted;
    use crate::RecordBatch;
    use crate::array::ValidityMask;
    use crate::batch::{ColumnVector, DecimalUnits, LazyText, SelectionMask, TypedValues};
    use crate::collation::Collation;
    use crate::execution::MemoryTracker;
    use crate::execution::sort::compare_sort_rows;

    const ROWS: usize = 40;

    /// A small deterministic spread with repeats, so keys tie.
    fn spread(seed: usize, row: usize, modulus: i64) -> i64 {
        let mixed = (row * 7_919 + seed * 104_729) % 1_000_003;
        i64::try_from(mixed).expect("small") % modulus - modulus / 2
    }

    fn mask(seed: usize, every: usize) -> Vec<bool> {
        (0..ROWS)
            .map(|row| !(row + seed).is_multiple_of(every))
            .collect()
    }

    /// Columns: integer, decimal(8,2), datetime(3), text, ENUM-labelled
    /// text (compared as values), a float (compared as values), and a
    /// payload integer. Row 5 of each batch is unselected.
    fn batch(seed: usize) -> RecordBatch {
        let valid = mask(seed, 6);
        let integers = (0..ROWS)
            .map(|row| spread(seed, row, 9))
            .collect::<Vec<_>>();
        let decimals = (0..ROWS)
            .map(|row| spread(seed + 1, row, 7) * 125)
            .collect::<Vec<_>>();
        let moments = (0..ROWS)
            .map(|row| 1_700_000_000_000_000 + spread(seed + 2, row, 5) * 1_000)
            .collect::<Vec<_>>();
        let words = ["b", "B", "a", "\u{e9}", "e", ""];
        let text = |row: usize| {
            words[usize::try_from(spread(seed + 3, row, 6).rem_euclid(6)).expect("index")]
        };
        let columns = vec![
            ColumnVector::from_typed(
                DataType::Int64,
                TypedValues::Int64(integers),
                ValidityMask::from_bools(&valid),
            ),
            ColumnVector::from_typed(
                DataType::Decimal {
                    precision: 8,
                    scale: 2,
                },
                TypedValues::Decimal128 {
                    values: DecimalUnits::Narrow(decimals),
                    scale: 2,
                    text: LazyText::decimal(2),
                },
                ValidityMask::from_bools(&mask(seed + 1, 5)),
            ),
            ColumnVector::from_typed(
                DataType::DateTime64 { fsp: 3 },
                TypedValues::Temporal {
                    units: moments,
                    text: LazyText::datetime(3),
                },
                ValidityMask::from_bools(&mask(seed + 2, 7)),
            ),
            ColumnVector::new(
                DataType::Utf8,
                (0..ROWS)
                    .map(|row| {
                        if row % 9 == 4 {
                            Value::Null
                        } else {
                            Value::Utf8(text(row).to_owned())
                        }
                    })
                    .collect(),
            )
            .expect("text"),
            ColumnVector::new(
                DataType::Utf8,
                (0..ROWS)
                    .map(|row| {
                        let index = u64::try_from(spread(seed + 4, row, 3).rem_euclid(3))
                            .expect("index")
                            + 1;
                        Value::Enum {
                            index,
                            label: ["low", "high", "mid"]
                                [usize::try_from(index - 1).expect("slot")]
                            .to_owned(),
                        }
                    })
                    .collect(),
            )
            .expect("enum"),
            ColumnVector::new(
                DataType::Float64,
                (0..ROWS)
                    .map(|row| {
                        #[allow(clippy::cast_precision_loss)]
                        Value::float64(spread(seed + 5, row, 11) as f64 / 4.0)
                    })
                    .collect(),
            )
            .expect("float"),
            ColumnVector::new(
                DataType::Int64,
                (0..ROWS)
                    .map(|row| Value::Int64(i64::try_from(seed * 1_000 + row).expect("small")))
                    .collect(),
            )
            .expect("payload"),
        ];
        let mut batch = RecordBatch::new(ROWS, columns).expect("batch");
        let mut selection = SelectionMask::all(ROWS);
        selection.set(5, false).expect("row");
        batch.set_selection(selection).expect("selection");
        batch
    }

    fn key(index: usize, ascending: bool, nulls_first: bool) -> BoundOrderKey {
        BoundOrderKey {
            index,
            ascending,
            nulls_first,
            decimal: index == 1,
            collation: None,
        }
    }

    fn rows_of(batch: &RecordBatch) -> Vec<Vec<Value>> {
        batch
            .selection()
            .selected_rows()
            .map(|row| {
                batch
                    .columns()
                    .iter()
                    .map(|column| column.value(row).cloned().unwrap_or(Value::Null))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn sorts_as_the_row_sort_does_for_every_kind_of_key() {
        let batches = (0..3).map(batch).collect::<Vec<_>>();
        let memory = MemoryTracker::new(usize::MAX);
        let types = batches[0]
            .columns()
            .iter()
            .map(ColumnVector::data_type)
            .collect::<Vec<_>>();
        let orders: Vec<Vec<BoundOrderKey>> = vec![
            vec![key(0, true, true)],
            vec![key(0, false, true), key(6, true, false)],
            vec![key(1, true, false)],
            vec![key(1, false, false), key(0, true, true)],
            vec![key(2, true, true)],
            vec![key(2, false, false)],
            vec![key(3, true, true)],
            vec![key(3, false, false), key(0, false, true)],
            vec![key(4, true, true), key(3, true, true)],
            vec![key(5, false, true)],
            vec![key(0, true, true), key(1, true, true), key(2, true, true)],
        ];
        for collation in ["utf8mb4_0900_ai_ci", "utf8mb4_general_ci", "utf8mb4_bin"] {
            let collation = Collation::from_mysql_name(collation).expect("collation");
            for keys in &orders {
                let mut expected = batches.iter().flat_map(rows_of).collect::<Vec<_>>();
                expected.sort_by(|left, right| compare_sort_rows(left, right, keys, collation));
                for limit in [None, Some(0), Some(1), Some(17), Some(200)] {
                    let mut sorted =
                        ColumnarSorted::new(batches.clone(), keys, None, collation, limit)
                            .expect("sorted");
                    let mut actual = Vec::new();
                    while let Some(batch) = sorted.next_batch(&types, &memory).expect("batch") {
                        actual.extend(rows_of(&batch));
                    }
                    let expected = &expected[..limit.unwrap_or(usize::MAX).min(expected.len())];
                    assert_eq!(actual, expected, "{keys:?} {limit:?} under {collation:?}");
                }
            }
        }
    }

    #[test]
    fn serves_rows_and_drops_the_sort_only_columns() {
        let batches = (0..2).map(batch).collect::<Vec<_>>();
        let keys = [key(0, true, true), key(6, false, true)];
        let collation = Collation::default();
        let mut expected = batches.iter().flat_map(rows_of).collect::<Vec<_>>();
        expected.sort_by(|left, right| compare_sort_rows(left, right, &keys, collation));
        for row in &mut expected {
            row.truncate(3);
        }
        let mut sorted =
            ColumnarSorted::new(batches, &keys, Some(3), collation, None).expect("sorted");
        let mut actual = Vec::new();
        while let Some(row) = sorted.next_row() {
            actual.push(row);
        }
        assert_eq!(actual, expected);
    }
}
