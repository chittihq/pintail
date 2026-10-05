//! Per-column planner statistics assembled from a snapshot's segment
//! statistics and its unflushed rows.

use pintail_catalog::{ColumnFacts, ColumnRange, ColumnStatistics, RangeDomain};

use super::TableSnapshot;
use crate::{
    segment::{NativeUnits, SmaExtremes},
    sketch::DistinctSketch,
};

/// Unflushed rows folded into the sketches at most. More are sampled
/// evenly, so a large memtable costs a catalog build no more than this.
const MEMTABLE_SAMPLE_ROWS: usize = 4_096;

impl TableSnapshot {
    /// Per-column statistics for the planner: every segment's distinct
    /// sketch merged, value ranges widened across segments, and the
    /// unflushed rows folded in (an even sample of them when there are
    /// many). Segments recorded before sketches existed leave a column's
    /// distinct count unknown rather than guessed. Superseded row versions
    /// still stored in older segments are counted too; the figures order
    /// joins and never answer a query.
    #[must_use]
    pub fn column_statistics(&self) -> ColumnStatistics {
        let columns = self.schema.columns();
        let mut rows = 0_u64;
        let mut non_null = vec![0_u64; columns.len()];
        let mut sketches: Vec<Option<DistinctSketch>> =
            vec![Some(DistinctSketch::default()); columns.len()];
        let mut ranges: Vec<Option<Option<ColumnRange>>> = vec![None; columns.len()];
        // A segment records temporal extremes only when every value parsed
        // as a real calendar date, so their presence proves it.
        let calendars = columns
            .iter()
            .map(|column| {
                NativeUnits::for_data_type(column.data_type()).filter(|units| {
                    matches!(units, NativeUnits::Date | NativeUnits::DateTime { .. })
                })
            })
            .collect::<Vec<_>>();
        let mut exact = calendars.iter().map(Option::is_some).collect::<Vec<_>>();
        for segment in &self.manifest.segments {
            let Some(smas) = &segment.smas else {
                rows = rows.saturating_add(segment.row_count);
                sketches.fill(None);
                ranges.fill(Some(None));
                exact.fill(false);
                continue;
            };
            rows = rows.saturating_add(smas.live_rows);
            for (index, column) in columns.iter().enumerate() {
                let Some(sma) = smas.columns.iter().find(|sma| sma.column_id == column.id()) else {
                    // Added after this segment was written: every one of
                    // its rows reads the column's fill, or NULL.
                    if let Some(fill) = column.absent_fill() {
                        non_null[index] = non_null[index].saturating_add(smas.live_rows);
                        if let Some(sketch) = &mut sketches[index] {
                            sketch.insert(fill);
                        }
                        ranges[index] = Some(widened(ranges[index], fill_range(fill)));
                        if let (Some(units), pintail_types::Value::Utf8(text)) =
                            (calendars[index], fill)
                            && units.parse_exact(text).is_none()
                        {
                            exact[index] = false;
                        }
                    }
                    continue;
                };
                if sma.non_null > 0 && sma.extremes.is_none() {
                    exact[index] = false;
                }
                non_null[index] = non_null[index].saturating_add(sma.non_null);
                match (&mut sketches[index], &sma.distinct) {
                    (Some(sketch), Some(theirs)) => sketch.merge(theirs),
                    (sketch, _) => *sketch = None,
                }
                if sma.non_null > 0 {
                    ranges[index] = Some(widened(ranges[index], sma.extremes.and_then(range_of)));
                }
            }
        }
        let live = self.memtable.values().filter(|row| !row.is_deleted());
        let memtable_rows = live.clone().count();
        rows = rows.saturating_add(u64::try_from(memtable_rows).unwrap_or(u64::MAX));
        // Every unflushed row, not a sample: one row a calendar rejects is
        // enough to need the per-value check.
        for row in live.clone() {
            for (index, value) in row.values().iter().enumerate().take(columns.len()) {
                if let (true, Some(units), pintail_types::Value::Utf8(text)) =
                    (exact[index], calendars[index], value)
                    && units.parse_exact(text).is_none()
                {
                    exact[index] = false;
                }
            }
        }
        let stride = memtable_rows.div_ceil(MEMTABLE_SAMPLE_ROWS).max(1);
        let scale = u64::try_from(stride).unwrap_or(1);
        for row in live.step_by(stride) {
            for (index, value) in row.values().iter().enumerate().take(columns.len()) {
                if matches!(value, pintail_types::Value::Null) {
                    continue;
                }
                non_null[index] = non_null[index].saturating_add(scale);
                if let Some(sketch) = &mut sketches[index] {
                    sketch.insert(value);
                }
            }
        }
        ColumnStatistics {
            rows,
            columns: columns
                .iter()
                .enumerate()
                .map(|(index, column)| ColumnFacts {
                    column_id: column.id(),
                    non_null: non_null[index],
                    distinct: sketches[index]
                        .as_ref()
                        .map(|sketch| sketch.estimate().min(non_null[index])),
                    range: ranges[index].flatten(),
                    calendar_exact: exact[index],
                })
                .collect(),
        }
    }
}

/// The range known so far (`None` when nothing is known yet, `Some(None)`
/// when it cannot be known) widened by one more segment's.
#[allow(clippy::option_option)] // the shape the caller keeps its ranges in
fn widened(known: Option<Option<ColumnRange>>, range: Option<ColumnRange>) -> Option<ColumnRange> {
    match (known, range) {
        (None, range) => range,
        (Some(Some(known)), Some(range)) if known.domain == range.domain => Some(ColumnRange {
            domain: known.domain,
            low: known.low.min(range.low),
            high: known.high.max(range.high),
        }),
        _ => None,
    }
}

/// The range of a segment whose every row reads one integer fill.
fn fill_range(fill: &pintail_types::Value) -> Option<ColumnRange> {
    let (domain, value) = match fill {
        pintail_types::Value::Int64(value) => (RangeDomain::Int, i128::from(*value)),
        pintail_types::Value::UInt64(value) => (RangeDomain::UInt, i128::from(*value)),
        _ => return None,
    };
    Some(ColumnRange {
        domain,
        low: value,
        high: value,
    })
}

fn range_of(extremes: SmaExtremes) -> Option<ColumnRange> {
    let (domain, low, high) = match extremes {
        SmaExtremes::Int { min, max } => (RangeDomain::Int, i128::from(min), i128::from(max)),
        SmaExtremes::UInt { min, max } => (RangeDomain::UInt, i128::from(min), i128::from(max)),
        SmaExtremes::DecimalUnits { min, max, scale } => (RangeDomain::Decimal { scale }, min, max),
        SmaExtremes::Temporal { min, max, units } => (
            match units {
                NativeUnits::Date => RangeDomain::Date,
                NativeUnits::DateTime { .. } => RangeDomain::DateTime,
                NativeUnits::Decimal { .. } => return None,
            },
            i128::from(min),
            i128::from(max),
        ),
        SmaExtremes::Float { .. } => return None,
    };
    Some(ColumnRange { domain, low, high })
}
